use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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
            "open-wake-job-test-{}-{timestamp}-{sequence}",
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

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_open-wake")
}

fn spawn_stop_hook(state: &Path, session: &str) -> Child {
    let mut child = Command::new(binary())
        .args(["hook", "--state-dir"])
        .arg(state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{{\"session_id\":\"{session}\",\"hook_event_name\":\"Stop\"}}"
    )
    .unwrap();
    child
}

fn stop_hook(state: &Path, session: &str) -> Value {
    let child = spawn_stop_hook(state, session);
    let output = child.wait_with_output().unwrap();
    successful_json(output)
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn successful_json(output: Output) -> Value {
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_json(state: &Path, jobs: &Path, session: &str, timeout: &str, script: &str) -> Value {
    successful_json(
        Command::new(binary())
            .args(["run", "--thread", session, "--state-dir"])
            .arg(state)
            .arg("--job-dir")
            .arg(jobs)
            .args(["--timeout", timeout, "--json", "--", "sh", "-c", script])
            .output()
            .unwrap(),
    )
}

fn status_json(state: &Path, session: &str) -> Value {
    successful_json(
        Command::new(binary())
            .args(["status", "--thread", session, "--state-dir"])
            .arg(state)
            .arg("--json")
            .output()
            .unwrap(),
    )
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "{} did not appear", path.display());
}

#[test]
fn run_survives_the_launcher_and_checkpoints_without_restarting() {
    let workspace = TestDir::new();
    let state = workspace.as_ref().join("state");
    let jobs = workspace.as_ref().join("jobs");
    let release = workspace.as_ref().join("release");
    let starts = workspace.as_ref().join("starts");
    let script = format!(
        "printf 'start\\n' >>'{}'; while ! test -e '{}'; do sleep 0.05; done; printf 'done\\n'; exit 7",
        starts.display(),
        release.display()
    );
    let launch = successful_json(
        Command::new(binary())
            .args(["run", "--thread", "job-flow", "--state-dir"])
            .arg(&state)
            .arg("--job-dir")
            .arg(&jobs)
            .args([
                "--timeout",
                "5s",
                "--check-every",
                "150ms",
                "--interval",
                "50ms",
                "--json",
                "--",
                "sh",
                "-c",
                &script,
            ])
            .output()
            .unwrap(),
    );
    let job_id = launch["job_id"].as_str().unwrap();
    let log_path = PathBuf::from(launch["log_path"].as_str().unwrap());

    let checkpoint = stop_hook(&state, "job-flow");
    assert_eq!(checkpoint["decision"], "block");
    assert!(
        checkpoint["reason"]
            .as_str()
            .unwrap()
            .contains("progress checkpoint 1")
    );

    wait_for_file(&starts);
    assert_eq!(fs::read_to_string(&starts).unwrap(), "start\n");
    fs::write(&release, b"").unwrap();

    let result_path = jobs.join(job_id).join("result.json");
    wait_for_file(&result_path);

    let completed = stop_hook(&state, "job-flow");
    let reason = completed["reason"].as_str().unwrap();
    assert!(reason.contains("condition met"));
    assert!(reason.contains("exit code 7"));
    assert_eq!(fs::read_to_string(&starts).unwrap(), "start\n");
    assert_eq!(fs::read_to_string(&log_path).unwrap(), "done\n");

    let logs = Command::new(binary())
        .args(["logs", job_id, "--job-dir"])
        .arg(&jobs)
        .output()
        .unwrap();
    assert!(logs.status.success());
    assert_eq!(
        String::from_utf8(logs.stdout).unwrap(),
        format!("{}\n", log_path.display())
    );

    let status = status_json(&state, "job-flow");
    assert_eq!(status["status"], "succeeded");
    assert_eq!(status["checkpoints"], 1);
    assert_eq!(status["job_status"]["state"], "completed");
    assert_eq!(status["job_status"]["result"]["exit_code"], 7);
}

#[test]
fn recorded_job_completion_takes_precedence_over_a_late_deadline_check() {
    let workspace = TestDir::new();
    let state = workspace.as_ref().join("state");
    let jobs = workspace.as_ref().join("jobs");
    let launch = run_json(&state, &jobs, "late-result", "100ms", "exit 7");
    let result = jobs
        .join(launch["job_id"].as_str().unwrap())
        .join("result.json");
    wait_for_file(&result);

    let completed = stop_hook(&state, "late-result");
    let reason = completed["reason"].as_str().unwrap();
    assert!(reason.contains("condition met"));
    assert!(reason.contains("exit code 7"));
    assert!(!reason.contains("deadline reached"));
}

#[test]
fn cancel_releases_a_session_when_the_stop_hook_was_never_invoked() {
    let workspace = TestDir::new();
    let state = workspace.as_ref().join("state");
    let jobs = workspace.as_ref().join("jobs");

    let first = run_json(&state, &jobs, "missed-hook", "5s", "exit 0");
    let first_result = jobs
        .join(first["job_id"].as_str().unwrap())
        .join("result.json");
    wait_for_file(&first_result);

    let cancelled = Command::new(binary())
        .args(["cancel", "--thread", "missed-hook", "--state-dir"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(cancelled.status.success());
    let cancelled_output = String::from_utf8(cancelled.stdout).unwrap();
    assert!(cancelled_output.contains("cancelled condition"));
    assert!(cancelled_output.contains("supervised job remains unchanged"));

    let cancelled_status = status_json(&state, "missed-hook");
    assert_eq!(cancelled_status["status"], "cancelled");
    assert_eq!(cancelled_status["attempts"], 0);
    assert_eq!(cancelled_status["job_status"]["state"], "completed");

    let second = run_json(&state, &jobs, "missed-hook", "5s", "exit 0");
    assert_ne!(second["condition_id"], first["condition_id"]);
    assert_ne!(second["job_id"], first["job_id"]);

    let second_result = jobs
        .join(second["job_id"].as_str().unwrap())
        .join("result.json");
    wait_for_file(&second_result);
}

#[test]
fn stale_hook_process_cannot_overwrite_a_replacement_condition() {
    let workspace = TestDir::new();
    let state = workspace.as_ref().join("state");
    let started = workspace.as_ref().join("started");
    let predicate = format!("touch '{}'; sleep 0.3", started.display());
    let armed = Command::new(binary())
        .args(["arm", "--thread", "process-race", "--state-dir"])
        .arg(&state)
        .args([
            "--timeout",
            "2s",
            "--interval",
            "50ms",
            "--check-timeout",
            "1s",
            "--",
            "sh",
            "-c",
            &predicate,
        ])
        .output()
        .unwrap();
    assert!(
        armed.status.success(),
        "{}",
        String::from_utf8_lossy(&armed.stderr)
    );

    let hook = spawn_stop_hook(&state, "process-race");
    wait_for_file(&started);

    let cancelled = Command::new(binary())
        .args(["cancel", "--thread", "process-race", "--state-dir"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(cancelled.status.success());
    let replacement = Command::new(binary())
        .args(["arm", "--thread", "process-race", "--state-dir"])
        .arg(&state)
        .args([
            "--timeout",
            "2s",
            "--interval",
            "50ms",
            "--check-timeout",
            "1s",
            "--",
            "sh",
            "-c",
            "exit 1",
        ])
        .output()
        .unwrap();
    assert!(
        replacement.status.success(),
        "{}",
        String::from_utf8_lossy(&replacement.stderr)
    );

    let stale_hook = hook.wait_with_output().unwrap();
    assert!(stale_hook.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&stale_hook.stdout).unwrap(),
        json!({})
    );

    let status = status_json(&state, "process-race");
    assert_eq!(status["status"], "armed");
    assert_eq!(status["attempts"], 0);
}

#[test]
fn status_preserves_condition_evidence_when_job_state_is_missing() {
    let workspace = TestDir::new();
    let state = workspace.as_ref().join("state");
    let jobs = workspace.as_ref().join("jobs");
    let launched = run_json(&state, &jobs, "missing-job", "5s", "exit 0");
    let job_dir = jobs.join(launched["job_id"].as_str().unwrap());
    let result = job_dir.join("result.json");
    wait_for_file(&result);
    fs::remove_dir_all(job_dir).unwrap();

    let status = status_json(&state, "missing-job");
    assert_eq!(status["status"], "armed");
    assert_eq!(status["attempts"], 0);
    assert!(status.get("job_status").is_none());
    assert!(
        status["job_error"]
            .as_str()
            .unwrap()
            .contains("job specification")
    );
}

#[test]
fn rejected_second_run_discards_its_prepared_job() {
    let workspace = TestDir::new();
    let state = workspace.as_ref().join("state");
    let jobs = workspace.as_ref().join("jobs");
    let armed = Command::new(binary())
        .args(["arm", "--thread", "active-session", "--state-dir"])
        .arg(&state)
        .args(["--timeout", "5s", "--", "sh", "-c", "exit 1"])
        .output()
        .unwrap();
    assert!(armed.status.success());

    let rejected = Command::new(binary())
        .args(["run", "--thread", "active-session", "--state-dir"])
        .arg(&state)
        .arg("--job-dir")
        .arg(&jobs)
        .args(["--timeout", "5s", "--", "sh", "-c", "exit 0"])
        .output()
        .unwrap();

    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("already active"));
    assert!(fs::read_dir(jobs).unwrap().next().is_none());
}

#[test]
fn supervisor_launch_failure_cancels_the_armed_condition() {
    let workspace = TestDir::new();
    let state = workspace.as_ref().join("state");
    let jobs = workspace.as_ref().join("jobs");
    let copied_binary = workspace.as_ref().join("open-wake-copy");
    fs::copy(binary(), &copied_binary).unwrap();
    fs::set_permissions(&copied_binary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(&state).unwrap();
    let condition_lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(state.join(".launch-failure.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(condition_lock.as_raw_fd(), libc::LOCK_EX) },
        0
    );

    let child = Command::new(&copied_binary)
        .args(["run", "--thread", "launch-failure", "--state-dir"])
        .arg(&state)
        .arg("--job-dir")
        .arg(&jobs)
        .args(["--timeout", "5s", "--", "sh", "-c", "exit 0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let prepared_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if fs::read_dir(&jobs).is_ok_and(|mut entries| entries.next().is_some()) {
            break;
        }
        assert!(
            Instant::now() < prepared_deadline,
            "run did not prepare its job before the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    }

    fs::remove_file(copied_binary).unwrap();
    drop(condition_lock);
    let failed = child.wait_with_output().unwrap();

    assert!(!failed.status.success());
    let stderr = String::from_utf8(failed.stderr).unwrap();
    assert!(stderr.contains("cancelled condition"), "{stderr}");
    assert!(stderr.contains("retained failed job record"), "{stderr}");

    let status = status_json(&state, "launch-failure");
    assert_eq!(status["status"], "cancelled");
    assert_eq!(status["job_status"]["state"], "supervisor_failed");
}
