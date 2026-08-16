use open_wake::{
    ArmRequest, ConditionStatus, HookResult, StopHookInput, arm, cancel, cancel_if_current,
    handle_stop_hook, status,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "open-wake-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn input(session_id: &str) -> StopHookInput {
    StopHookInput {
        session_id: session_id.to_owned(),
        hook_event_name: "Stop".to_owned(),
    }
}

fn request(cwd: &Path, session_id: &str, command: Vec<String>) -> ArmRequest {
    ArmRequest {
        session_id: session_id.to_owned(),
        label: Some("integration check".to_owned()),
        cwd: cwd.to_owned(),
        command,
        timeout: Duration::from_secs(2),
        interval: Duration::from_millis(50),
        check_timeout: Duration::from_secs(1),
        check_every: None,
        job: None,
    }
}

fn set_record_status(state: &Path, session_id: &str, status: &str) {
    let path = state.join(format!("{session_id}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["status"] = status.into();
    fs::write(path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
}

#[test]
fn checkpoint_wakes_without_resolving_or_restarting_the_condition() {
    let state = TestDir::new();
    let ready = state.as_ref().join("ready");
    let attempts = state.as_ref().join("attempts");
    let mut request = request(
        state.as_ref(),
        "thread-checkpoint",
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                "printf x >>'{}'; test -e '{}'",
                attempts.display(),
                ready.display()
            ),
        ],
    );
    request.timeout = Duration::from_secs(3);
    request.check_every = Some(Duration::from_millis(150));
    arm(state.as_ref(), request).unwrap();

    let HookResult::Continue(reason) =
        handle_stop_hook(state.as_ref(), &input("thread-checkpoint")).unwrap()
    else {
        panic!("expected checkpoint continuation");
    };
    assert!(reason.contains("progress checkpoint 1"));
    let condition = status(state.as_ref(), "thread-checkpoint")
        .unwrap()
        .unwrap();
    assert_eq!(condition.status, ConditionStatus::Armed);
    assert_eq!(condition.checkpoints, 1);

    fs::write(&ready, b"").unwrap();
    let HookResult::Continue(reason) =
        handle_stop_hook(state.as_ref(), &input("thread-checkpoint")).unwrap()
    else {
        panic!("expected completion continuation");
    };
    assert!(reason.contains("condition met"));
    assert_eq!(
        status(state.as_ref(), "thread-checkpoint")
            .unwrap()
            .unwrap()
            .status,
        ConditionStatus::Succeeded
    );
    assert!(fs::read_to_string(attempts).unwrap().len() >= 2);
}

#[test]
fn successful_predicate_becomes_one_continuation() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-success",
            vec!["sh".into(), "-c".into(), "printf ready".into()],
        ),
    )
    .unwrap();

    let result = handle_stop_hook(state.as_ref(), &input("thread-success")).unwrap();
    let HookResult::Continue(reason) = result else {
        panic!("expected continuation");
    };
    assert!(reason.contains("condition met"));
    assert!(reason.contains("ready"));
    assert!(reason.contains("Last exit code: 0."));
    assert!(!reason.contains("Some(0)"));

    let condition = status(state.as_ref(), "thread-success").unwrap().unwrap();
    assert_eq!(condition.status, ConditionStatus::Succeeded);
    assert_eq!(condition.attempts, 1);
    assert_eq!(
        handle_stop_hook(state.as_ref(), &input("thread-success")).unwrap(),
        HookResult::Noop
    );
}

#[test]
fn false_predicate_continues_at_deadline_with_last_result() {
    let state = TestDir::new();
    let mut request = request(
        state.as_ref(),
        "thread-timeout",
        vec!["sh".into(), "-c".into(), "printf not-yet; exit 1".into()],
    );
    request.timeout = Duration::from_secs(2);
    arm(state.as_ref(), request).unwrap();

    let result = handle_stop_hook(state.as_ref(), &input("thread-timeout")).unwrap();
    let HookResult::Continue(reason) = result else {
        panic!("expected deadline continuation");
    };
    assert!(reason.contains("deadline reached"));
    assert!(reason.contains("not-yet"));

    let condition = status(state.as_ref(), "thread-timeout").unwrap().unwrap();
    assert_eq!(condition.status, ConditionStatus::TimedOut);
    assert!(condition.attempts >= 1);
}

#[test]
fn cancellation_is_terminal_without_a_stop_hook_and_allows_rearming() {
    let state = TestDir::new();
    let cancelled = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-cancel",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    )
    .unwrap();
    let condition = cancel(state.as_ref(), "thread-cancel").unwrap();

    assert_eq!(condition.status, ConditionStatus::Cancelled);
    assert!(condition.resolved_at_ms.is_some());
    let repeated = cancel(state.as_ref(), "thread-cancel").unwrap();
    assert_eq!(repeated.id, condition.id);
    assert_eq!(repeated.resolved_at_ms, condition.resolved_at_ms);
    assert_eq!(
        handle_stop_hook(state.as_ref(), &input("thread-cancel")).unwrap(),
        HookResult::Noop
    );

    let replacement = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-cancel",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    )
    .unwrap();
    assert_ne!(replacement.id, cancelled.id);
    assert_eq!(replacement.status, ConditionStatus::Armed);
}

#[test]
fn stale_hook_cannot_overwrite_a_rearmed_condition() {
    let state = TestDir::new();
    let state_path = state.as_ref().to_owned();
    let started = state.as_ref().join("started");
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-rearm",
            vec![
                "sh".into(),
                "-c".into(),
                format!("touch '{}'; sleep 0.3", started.display()),
            ],
        ),
    )
    .unwrap();

    let hook =
        thread::spawn(move || handle_stop_hook(&state_path, &input("thread-rearm")).unwrap());
    let started_deadline = Instant::now() + Duration::from_secs(2);
    while !started.exists() && Instant::now() < started_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(started.exists());

    cancel(state.as_ref(), "thread-rearm").unwrap();
    let replacement = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-rearm",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    );
    let hook_result = hook.join().unwrap();

    let replacement = replacement.expect("cancellation should release the session immediately");
    assert_eq!(hook_result, HookResult::Noop);
    let current = status(state.as_ref(), "thread-rearm").unwrap().unwrap();
    assert_eq!(current.id, replacement.id);
    assert_eq!(current.status, ConditionStatus::Armed);
    assert_eq!(current.attempts, 0);
}

#[test]
fn stale_launcher_cannot_cancel_a_rearmed_condition() {
    let state = TestDir::new();
    let original = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-launcher-race",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    )
    .unwrap();
    cancel(state.as_ref(), "thread-launcher-race").unwrap();
    let replacement = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-launcher-race",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    )
    .unwrap();

    assert!(
        cancel_if_current(state.as_ref(), "thread-launcher-race", &original.id,)
            .unwrap()
            .is_none()
    );
    let current = status(state.as_ref(), "thread-launcher-race")
        .unwrap()
        .unwrap();
    assert_eq!(current.id, replacement.id);
    assert_eq!(current.status, ConditionStatus::Armed);
}

#[test]
fn a_second_hook_cannot_observe_the_same_condition_concurrently() {
    let state = TestDir::new();
    let state_path = state.as_ref().to_owned();
    let started = state.as_ref().join("started");
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-single-waiter",
            vec![
                "sh".into(),
                "-c".into(),
                format!("touch '{}'; sleep 0.3; exit 1", started.display()),
            ],
        ),
    )
    .unwrap();

    let first = thread::spawn(move || {
        handle_stop_hook(&state_path, &input("thread-single-waiter")).unwrap()
    });
    let started_deadline = Instant::now() + Duration::from_secs(2);
    while !started.exists() && Instant::now() < started_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(started.exists());

    let duplicate = handle_stop_hook(state.as_ref(), &input("thread-single-waiter")).unwrap();
    assert_eq!(duplicate, HookResult::Noop);

    cancel(state.as_ref(), "thread-single-waiter").unwrap();
    assert_eq!(first.join().unwrap(), HookResult::Noop);
}

#[test]
fn concurrent_arms_create_exactly_one_active_condition() {
    const CONTENDERS: usize = 8;

    let state = TestDir::new();
    let barrier = Arc::new(Barrier::new(CONTENDERS));
    let contenders = (0..CONTENDERS)
        .map(|_| {
            let state = state.as_ref().to_owned();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                arm(
                    &state,
                    request(
                        &state,
                        "thread-concurrent-arm",
                        vec!["sh".into(), "-c".into(), "exit 1".into()],
                    ),
                )
            })
        })
        .collect::<Vec<_>>();
    let results = contenders
        .into_iter()
        .map(|contender| contender.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| error.contains("already active"))
    );
    assert_eq!(
        status(state.as_ref(), "thread-concurrent-arm")
            .unwrap()
            .unwrap()
            .status,
        ConditionStatus::Armed
    );
}

#[test]
fn legacy_cancel_requested_record_is_terminal_and_can_be_normalized() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-legacy-cancel",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    )
    .unwrap();
    set_record_status(state.as_ref(), "thread-legacy-cancel", "cancel_requested");

    assert_eq!(
        status(state.as_ref(), "thread-legacy-cancel")
            .unwrap()
            .unwrap()
            .status,
        ConditionStatus::CancelRequested
    );
    let normalized = cancel(state.as_ref(), "thread-legacy-cancel").unwrap();
    assert_eq!(normalized.status, ConditionStatus::Cancelled);
    assert!(normalized.resolved_at_ms.is_some());
    assert!(
        arm(
            state.as_ref(),
            request(
                state.as_ref(),
                "thread-legacy-cancel",
                vec!["sh".into(), "-c".into(), "exit 1".into()],
            ),
        )
        .is_ok()
    );
}

#[test]
fn only_armed_and_waiting_records_block_rearming() {
    let state = TestDir::new();
    for (index, status_name) in [
        "cancel_requested",
        "succeeded",
        "timed_out",
        "failed",
        "cancelled",
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("terminal-{index}");
        arm(
            state.as_ref(),
            request(
                state.as_ref(),
                &session,
                vec!["sh".into(), "-c".into(), "exit 1".into()],
            ),
        )
        .unwrap();
        set_record_status(state.as_ref(), &session, status_name);
        assert!(
            arm(
                state.as_ref(),
                request(
                    state.as_ref(),
                    &session,
                    vec!["sh".into(), "-c".into(), "exit 1".into()],
                ),
            )
            .is_ok(),
            "{status_name} should be terminal"
        );
    }

    for status_name in ["armed", "waiting"] {
        let session = format!("active-{status_name}");
        arm(
            state.as_ref(),
            request(
                state.as_ref(),
                &session,
                vec!["sh".into(), "-c".into(), "exit 1".into()],
            ),
        )
        .unwrap();
        set_record_status(state.as_ref(), &session, status_name);
        assert!(
            arm(
                state.as_ref(),
                request(
                    state.as_ref(),
                    &session,
                    vec!["sh".into(), "-c".into(), "exit 1".into()],
                ),
            )
            .is_err(),
            "{status_name} should remain active"
        );
    }
}

#[test]
fn cancellation_does_not_replace_non_cancel_terminal_outcomes() {
    let state = TestDir::new();
    for status_name in ["succeeded", "timed_out", "failed"] {
        let session = format!("resolved-{}", status_name.replace('_', "-"));
        arm(
            state.as_ref(),
            request(
                state.as_ref(),
                &session,
                vec!["sh".into(), "-c".into(), "exit 1".into()],
            ),
        )
        .unwrap();
        set_record_status(state.as_ref(), &session, status_name);
        assert!(cancel(state.as_ref(), &session).is_err());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &fs::read(state.as_ref().join(format!("{session}.json"))).unwrap()
            )
            .unwrap()["status"],
            status_name
        );
    }
}

#[test]
fn corrupt_condition_is_not_silently_replaced() {
    let state = TestDir::new();
    let record = state.as_ref().join("thread-corrupt.json");
    fs::write(&record, b"not-json\n").unwrap();

    let error = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-corrupt",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    )
    .unwrap_err();

    assert!(error.contains("read existing condition"));
    assert_eq!(fs::read(record).unwrap(), b"not-json\n");
}

#[test]
fn invalid_predicate_wakes_codex_with_the_error() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-error",
            vec!["definitely-not-a-real-open-wake-command".into()],
        ),
    )
    .unwrap();

    let result = handle_stop_hook(state.as_ref(), &input("thread-error")).unwrap();
    let HookResult::Continue(reason) = result else {
        panic!("expected error continuation");
    };
    assert!(reason.contains("condition check failed"));
    assert!(reason.contains("No such file or directory"));
    assert_eq!(
        status(state.as_ref(), "thread-error")
            .unwrap()
            .unwrap()
            .status,
        ConditionStatus::Failed
    );
    assert!(fs::read_dir(state.as_ref()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".last-check.log")
    }));
}

#[test]
fn check_timeout_kills_the_entire_predicate_process_group() {
    let state = TestDir::new();
    let first_check = state.as_ref().join("first-check");
    let child_pid = state.as_ref().join("child-pid");
    let mut request = request(
        state.as_ref(),
        "thread-process-group",
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                "if test -e '{}'; then exit 0; fi; : >'{}'; sleep 60 & printf '%s\\n' \"$!\" >'{}'; wait",
                first_check.display(),
                first_check.display(),
                child_pid.display()
            ),
        ],
    );
    request.timeout = Duration::from_secs(3);
    request.check_timeout = Duration::from_millis(200);
    arm(state.as_ref(), request).unwrap();

    let result = handle_stop_hook(state.as_ref(), &input("thread-process-group")).unwrap();
    assert!(matches!(result, HookResult::Continue(_)));
    let condition = status(state.as_ref(), "thread-process-group")
        .unwrap()
        .unwrap();
    assert_eq!(condition.status, ConditionStatus::Succeeded);
    assert_eq!(condition.attempts, 2);

    let child_pid = fs::read_to_string(child_pid)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let result = unsafe { libc::kill(child_pid, 0) };
        if result == -1 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a child process survived the predicate timeout"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
