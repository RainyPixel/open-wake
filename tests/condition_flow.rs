use codex_wake::{
    ArmRequest, ConditionStatus, HookResult, StopHookInput, arm, cancel, handle_stop_hook, status,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
            "codex-wake-test-{}-{timestamp}-{sequence}",
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
    }
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
    request.timeout = Duration::from_millis(180);
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
fn cancelled_condition_does_not_continue_the_agent() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-cancel",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        ),
    )
    .unwrap();
    cancel(state.as_ref(), "thread-cancel").unwrap();

    assert_eq!(
        handle_stop_hook(state.as_ref(), &input("thread-cancel")).unwrap(),
        HookResult::Noop
    );
    assert_eq!(
        status(state.as_ref(), "thread-cancel")
            .unwrap()
            .unwrap()
            .status,
        ConditionStatus::Cancelled
    );
}

#[test]
fn invalid_predicate_wakes_codex_with_the_error() {
    let state = TestDir::new();
    arm(
        state.as_ref(),
        request(
            state.as_ref(),
            "thread-error",
            vec!["definitely-not-a-real-codex-wake-command".into()],
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
}

#[test]
fn deadline_kills_the_entire_predicate_process_group() {
    let state = TestDir::new();
    let leaked_marker = state.as_ref().join("leaked-child");
    let mut request = request(
        state.as_ref(),
        "thread-process-group",
        vec![
            "sh".into(),
            "-c".into(),
            format!("(sleep 0.5; touch '{}') & wait", leaked_marker.display()),
        ],
    );
    request.timeout = Duration::from_millis(150);
    request.check_timeout = Duration::from_secs(2);
    arm(state.as_ref(), request).unwrap();

    let started = Instant::now();
    let result = handle_stop_hook(state.as_ref(), &input("thread-process-group")).unwrap();
    assert!(matches!(result, HookResult::Continue(_)));
    assert!(started.elapsed() < Duration::from_secs(1));

    std::thread::sleep(Duration::from_millis(600));
    assert!(
        !leaked_marker.exists(),
        "a child process survived the predicate deadline"
    );
}
