use open_wake::{
    ArmRequest, ConditionStatus, DeliveryStatus, HookResult, StopHookInput, WatcherState,
    acknowledge_hook_delivery, arm, cancel, cancel_if_current, deliver_hook_notification,
    handle_stop_hook, inspect_condition, status,
};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod common;

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
    input_for_turn(session_id, "test-turn")
}

fn input_for_turn(session_id: &str, turn_id: &str) -> StopHookInput {
    StopHookInput {
        session_id: session_id.to_owned(),
        turn_id: Some(turn_id.to_owned()),
        hook_event_name: "Stop".to_owned(),
    }
}

fn delivered_stop_hook(state: &Path, session_id: &str) -> HookResult {
    let input = input(session_id);
    let result = handle_stop_hook(state, &input).unwrap();
    if matches!(result, HookResult::Continue(_)) {
        assert!(acknowledge_hook_delivery(state, &input).unwrap());
    }
    result
}

fn request(cwd: &Path, session_id: &str, command: Vec<String>) -> ArmRequest {
    ArmRequest {
        session_id: session_id.to_owned(),
        label: Some("integration check".to_owned()),
        cwd: cwd.to_owned(),
        command,
        timeout: Duration::from_secs(2),
        poll_every: Duration::from_millis(50),
        check_timeout: Duration::from_secs(1),
        checkpoint_every: None,
        job: None,
    }
}

fn set_record_status(state: &Path, session_id: &str, status: &str) {
    let path = state.join(format!("{session_id}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["status"] = status.into();
    fs::write(path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
}

fn make_record_version_one(state: &Path, session_id: &str) {
    let path = state.join(format!("{session_id}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["version"] = 1.into();
    record["interval_ms"] = record["poll_every_ms"].clone();
    if let Some(checkpoint) = record.get("checkpoint_every_ms") {
        record["check_every_ms"] = checkpoint.clone();
    }
    record.as_object_mut().unwrap().remove("poll_every_ms");
    record
        .as_object_mut()
        .unwrap()
        .remove("checkpoint_every_ms");
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
}

fn age_file(path: &Path, seconds: i64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let time = libc::timeval {
        tv_sec: now - seconds,
        tv_usec: 0,
    };
    let times = [time, time];
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let result = unsafe { libc::utimes(path.as_ptr(), times.as_ptr()) };
    assert_eq!(result, 0);
}

fn make_hook_owner_stale(state: &Path, session_id: &str, condition_id: &str, attempts: u64) {
    let path = state.join(format!("{session_id}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["status"] = "waiting".into();
    record["attempts"] = attempts.into();
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let lease_path = state.join(format!(".{session_id}.owner.json"));
    let heartbeat = crate_now_ms().saturating_sub(30_000);
    let lease = serde_json::json!({
        "version": 1,
        "condition_id": condition_id,
        "owner_id": "interrupted-hook",
        "pid": 999999,
        "heartbeat_at_ms": heartbeat
    });
    fs::write(&lease_path, serde_json::to_vec_pretty(&lease).unwrap()).unwrap();
    fs::write(
        state.join(format!(
            "{session_id}.{condition_id}.interrupted-hook.last-check.log"
        )),
        "stale output",
    )
    .unwrap();
}

fn crate_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
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
    request.timeout = Duration::from_secs(120);
    request.checkpoint_every = Some(Duration::from_secs(60));
    arm(state.as_ref(), request).unwrap();
    common::force_checkpoint_now(state.as_ref(), "thread-checkpoint");

    let HookResult::Continue(reason) = delivered_stop_hook(state.as_ref(), "thread-checkpoint")
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
    let HookResult::Continue(reason) = delivered_stop_hook(state.as_ref(), "thread-checkpoint")
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

    let result = delivered_stop_hook(state.as_ref(), "thread-success");
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

    let result = delivered_stop_hook(state.as_ref(), "thread-timeout");
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
fn rearming_removes_a_stale_owner_from_the_previous_condition() {
    let state = TestDir::new();
    let original = arm(
        state.as_ref(),
        request(state.as_ref(), "thread-rearm-owner", vec!["true".into()]),
    )
    .unwrap();
    cancel(state.as_ref(), "thread-rearm-owner").unwrap();
    let owner_path = state.as_ref().join(".thread-rearm-owner.owner.json");
    fs::write(
        &owner_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "condition_id": original.id,
            "owner_id": "old-owner",
            "pid": 999_999,
            "heartbeat_at_ms": 0
        }))
        .unwrap(),
    )
    .unwrap();

    arm(
        state.as_ref(),
        request(state.as_ref(), "thread-rearm-owner", vec!["true".into()]),
    )
    .unwrap();
    assert!(!owner_path.exists());
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
fn an_interrupted_hook_is_recovered_by_the_next_stop_hook() {
    let state = TestDir::new();
    let ready = state.as_ref().join("recovery-ready");
    let armed = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-hook-recovery",
            vec![
                "sh".into(),
                "-c".into(),
                format!("test -e '{}'", ready.display()),
            ],
        ),
    )
    .unwrap();
    make_hook_owner_stale(state.as_ref(), "thread-hook-recovery", &armed.id, 7);

    let snapshot = inspect_condition(state.as_ref(), "thread-hook-recovery")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.watcher.state, WatcherState::Stale);

    fs::write(&ready, b"").unwrap();
    let HookResult::Continue(reason) = delivered_stop_hook(state.as_ref(), "thread-hook-recovery")
    else {
        panic!("expected the next Stop hook to recover the condition");
    };
    assert!(reason.contains("condition met"));

    let recovered = status(state.as_ref(), "thread-hook-recovery")
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id, armed.id);
    assert_eq!(recovered.status, ConditionStatus::Succeeded);
    assert_eq!(recovered.attempts, 8);
    assert!(
        !state
            .as_ref()
            .join(format!(
                "thread-hook-recovery.{}.interrupted-hook.last-check.log",
                armed.id
            ))
            .exists()
    );
    assert!(
        !state
            .as_ref()
            .join(".thread-hook-recovery.owner.json")
            .exists()
    );
}

#[test]
fn a_terminal_notification_is_retried_after_pre_delivery_interruption() {
    let state = TestDir::new();
    let attempts = state.as_ref().join("attempts");
    let armed = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-delivery-recovery",
            vec![
                "sh".into(),
                "-c".into(),
                format!("printf x >>'{}'; exit 0", attempts.display()),
            ],
        ),
    )
    .unwrap();

    let first_input = input_for_turn("thread-delivery-recovery", "interrupted-turn");
    let HookResult::Continue(first_reason) =
        handle_stop_hook(state.as_ref(), &first_input).unwrap()
    else {
        panic!("expected a pending terminal notification");
    };
    let pending = status(state.as_ref(), "thread-delivery-recovery")
        .unwrap()
        .unwrap();
    assert_eq!(pending.status, ConditionStatus::Waiting);
    assert_eq!(
        pending.pending_notification.as_ref().unwrap().kind,
        open_wake::NotificationKind::Succeeded
    );
    assert_eq!(fs::read_to_string(&attempts).unwrap(), "x");
    let delivery_error = deliver_hook_notification(state.as_ref(), &first_input, || {
        Err("simulated broken output pipe".to_owned())
    })
    .unwrap_err();
    assert!(delivery_error.contains("simulated broken output pipe"));
    assert!(
        status(state.as_ref(), "thread-delivery-recovery")
            .unwrap()
            .unwrap()
            .pending_notification
            .is_some()
    );

    make_hook_owner_stale(
        state.as_ref(),
        "thread-delivery-recovery",
        &armed.id,
        pending.attempts,
    );
    let recovered_input = input_for_turn("thread-delivery-recovery", "recovery-turn");
    let HookResult::Continue(recovered_reason) =
        handle_stop_hook(state.as_ref(), &recovered_input).unwrap()
    else {
        panic!("expected the pending notification to be retried");
    };
    assert_eq!(recovered_reason, first_reason);
    assert_eq!(fs::read_to_string(&attempts).unwrap(), "x");

    let mut obsolete_output_written = false;
    assert_eq!(
        deliver_hook_notification(state.as_ref(), &first_input, || {
            obsolete_output_written = true;
            Ok(())
        })
        .unwrap(),
        DeliveryStatus::Superseded
    );
    assert!(!obsolete_output_written);
    assert!(
        status(state.as_ref(), "thread-delivery-recovery")
            .unwrap()
            .unwrap()
            .pending_notification
            .is_some()
    );
    assert!(acknowledge_hook_delivery(state.as_ref(), &recovered_input).unwrap());

    let delivered = status(state.as_ref(), "thread-delivery-recovery")
        .unwrap()
        .unwrap();
    assert_eq!(delivered.status, ConditionStatus::Succeeded);
    assert!(delivered.pending_notification.is_none());
}

#[test]
fn a_checkpoint_is_retried_before_rearming_the_condition() {
    let state = TestDir::new();
    let attempts = state.as_ref().join("checkpoint-attempts");
    let mut request = request(
        state.as_ref(),
        "thread-checkpoint-delivery-recovery",
        vec![
            "sh".into(),
            "-c".into(),
            format!("printf x >>'{}'; exit 1", attempts.display()),
        ],
    );
    request.timeout = Duration::from_secs(120);
    request.checkpoint_every = Some(Duration::from_secs(60));
    let armed = arm(state.as_ref(), request).unwrap();
    common::force_checkpoint_now(state.as_ref(), "thread-checkpoint-delivery-recovery");

    let first_input = input_for_turn(
        "thread-checkpoint-delivery-recovery",
        "interrupted-checkpoint-turn",
    );
    let HookResult::Continue(first_reason) =
        handle_stop_hook(state.as_ref(), &first_input).unwrap()
    else {
        panic!("expected a pending checkpoint");
    };
    assert!(first_reason.contains("progress checkpoint 1"));
    let pending = status(state.as_ref(), "thread-checkpoint-delivery-recovery")
        .unwrap()
        .unwrap();
    assert_eq!(pending.status, ConditionStatus::Waiting);
    assert_eq!(pending.attempts, 1);

    make_hook_owner_stale(
        state.as_ref(),
        "thread-checkpoint-delivery-recovery",
        &armed.id,
        pending.attempts,
    );
    let recovered_input = input_for_turn(
        "thread-checkpoint-delivery-recovery",
        "recovered-checkpoint-turn",
    );
    let HookResult::Continue(recovered_reason) =
        handle_stop_hook(state.as_ref(), &recovered_input).unwrap()
    else {
        panic!("expected the checkpoint to be retried");
    };
    assert_eq!(recovered_reason, first_reason);
    assert_eq!(fs::read_to_string(attempts).unwrap(), "x");
    assert!(acknowledge_hook_delivery(state.as_ref(), &recovered_input).unwrap());

    let rearmed = status(state.as_ref(), "thread-checkpoint-delivery-recovery")
        .unwrap()
        .unwrap();
    assert_eq!(rearmed.status, ConditionStatus::Armed);
    assert_eq!(rearmed.checkpoints, 1);
    assert!(rearmed.pending_notification.is_none());
    assert!(rearmed.next_checkpoint_at_ms.unwrap() > crate_now_ms());
}

#[test]
fn cancellation_supersedes_a_pending_notification_before_output() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-cancel-delivery",
            vec!["true".into()],
        ),
    )
    .unwrap();
    let input = input("thread-cancel-delivery");
    assert!(matches!(
        handle_stop_hook(state.as_ref(), &input).unwrap(),
        HookResult::Continue(_)
    ));
    cancel(state.as_ref(), "thread-cancel-delivery").unwrap();

    let mut output_written = false;
    let delivery = deliver_hook_notification(state.as_ref(), &input, || {
        output_written = true;
        Ok(())
    })
    .unwrap();
    assert_eq!(delivery, DeliveryStatus::Superseded);
    assert!(!output_written);
    assert_eq!(
        status(state.as_ref(), "thread-cancel-delivery")
            .unwrap()
            .unwrap()
            .status,
        ConditionStatus::Cancelled
    );
}

#[test]
fn version_one_condition_fields_are_read_as_poll_and_checkpoint_fields() {
    let state = TestDir::new();
    let mut version_one = request(state.as_ref(), "thread-v1-state", vec!["true".into()]);
    version_one.timeout = Duration::from_secs(120);
    version_one.checkpoint_every = Some(Duration::from_secs(60));
    let armed = arm(state.as_ref(), version_one).unwrap();
    make_record_version_one(state.as_ref(), "thread-v1-state");

    let migrated = status(state.as_ref(), "thread-v1-state").unwrap().unwrap();
    assert_eq!(migrated.id, armed.id);
    assert_eq!(migrated.poll_every_ms, 50);
    assert_eq!(migrated.checkpoint_every_ms, Some(60_000));
}

#[test]
fn active_version_two_condition_is_migrated_before_delivery() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(state.as_ref(), "thread-version-two", vec!["true".into()]),
    )
    .unwrap();
    let path = state.as_ref().join("thread-version-two.json");
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["version"] = 2.into();
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    assert!(matches!(
        delivered_stop_hook(state.as_ref(), "thread-version-two"),
        HookResult::Continue(_)
    ));
    let migrated = status(state.as_ref(), "thread-version-two")
        .unwrap()
        .unwrap();
    assert_eq!(migrated.version, 3);
    assert_eq!(migrated.status, ConditionStatus::Succeeded);
}

#[test]
fn a_recent_version_one_waiting_record_is_not_stolen_from_a_legacy_hook() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(state.as_ref(), "thread-legacy-recent", vec!["true".into()]),
    )
    .unwrap();
    make_record_version_one(state.as_ref(), "thread-legacy-recent");
    set_record_status(state.as_ref(), "thread-legacy-recent", "waiting");
    let _ = fs::remove_file(state.as_ref().join(".thread-legacy-recent.owner.json"));

    let snapshot = inspect_condition(state.as_ref(), "thread-legacy-recent")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.watcher.state, WatcherState::LegacyUnknown);
    assert_eq!(
        handle_stop_hook(state.as_ref(), &input("thread-legacy-recent")).unwrap(),
        HookResult::Noop
    );
}

#[test]
fn a_stale_version_one_waiting_record_is_recovered_after_legacy_grace() {
    let state = TestDir::new();
    let ready = state.as_ref().join("legacy-ready");
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-legacy-stale",
            vec![
                "sh".into(),
                "-c".into(),
                format!("test -e '{}'", ready.display()),
            ],
        ),
    )
    .unwrap();
    make_record_version_one(state.as_ref(), "thread-legacy-stale");
    set_record_status(state.as_ref(), "thread-legacy-stale", "waiting");
    let condition_path = state.as_ref().join("thread-legacy-stale.json");
    age_file(&condition_path, 31);
    let _ = fs::remove_file(state.as_ref().join(".thread-legacy-stale.owner.json"));

    let snapshot = inspect_condition(state.as_ref(), "thread-legacy-stale")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.watcher.state, WatcherState::Stale);
    fs::write(&ready, b"").unwrap();
    let HookResult::Continue(reason) = delivered_stop_hook(state.as_ref(), "thread-legacy-stale")
    else {
        panic!("expected stale legacy condition recovery");
    };
    assert!(reason.contains("condition met"));
}

#[test]
fn a_rapid_legacy_checkpoint_is_migrated_to_the_current_minimum() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-legacy-checkpoint",
            vec!["true".into()],
        ),
    )
    .unwrap();
    make_record_version_one(state.as_ref(), "thread-legacy-checkpoint");
    let path = state.as_ref().join("thread-legacy-checkpoint.json");
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["timeout_ms"] = 120_000.into();
    record["check_every_ms"] = 150_u64.into();
    record["next_checkpoint_at_ms"] = (record["created_at_ms"].as_u64().unwrap() + 150).into();
    record["status"] = "waiting".into();
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    age_file(&path, 31);
    let _ = fs::remove_file(state.as_ref().join(".thread-legacy-checkpoint.owner.json"));

    let HookResult::Continue(reason) =
        delivered_stop_hook(state.as_ref(), "thread-legacy-checkpoint")
    else {
        panic!("expected legacy checkpoint migration to recover");
    };
    assert!(reason.contains("condition met"));

    let migrated = status(state.as_ref(), "thread-legacy-checkpoint")
        .unwrap()
        .unwrap();
    assert_eq!(migrated.version, 3);
    assert_eq!(migrated.checkpoint_every_ms, Some(60_000));
    assert!(migrated.next_checkpoint_at_ms.unwrap() > migrated.created_at_ms + 150);
}

#[test]
fn unsupported_condition_versions_fail_closed() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(state.as_ref(), "thread-future-state", vec!["true".into()]),
    )
    .unwrap();
    let path = state.as_ref().join("thread-future-state.json");
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["version"] = 4.into();
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let error = status(state.as_ref(), "thread-future-state").unwrap_err();
    assert!(
        error.contains("unsupported condition version"),
        "unexpected error: {error}"
    );
}

#[test]
fn inspecting_a_missing_condition_does_not_create_state_files() {
    let state = TestDir::new();
    let snapshot = inspect_condition(state.as_ref(), "thread-missing").unwrap();
    assert!(snapshot.is_none());
    assert_eq!(fs::read_dir(state.as_ref()).unwrap().count(), 0);
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
fn inconsistent_pending_checkpoint_is_rejected() {
    let state = TestDir::new();
    let armed = arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-invalid-pending-checkpoint",
            vec!["true".into()],
        ),
    )
    .unwrap();
    let record_path = state
        .as_ref()
        .join("thread-invalid-pending-checkpoint.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    record["status"] = "waiting".into();
    record["pending_notification"] = serde_json::json!({
        "kind": "checkpoint",
        "created_at_ms": armed.created_at_ms
    });
    fs::write(&record_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let error = status(state.as_ref(), "thread-invalid-pending-checkpoint").unwrap_err();
    assert!(error.contains("pending checkpoint requires checkpoint state"));
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

    let result = delivered_stop_hook(state.as_ref(), "thread-error");
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

    let result = delivered_stop_hook(state.as_ref(), "thread-process-group");
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
