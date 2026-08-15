use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

fn stop_hook(state: &Path, session: &str) -> Value {
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
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
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
    let output = Command::new(binary())
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
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let launch: Value = serde_json::from_slice(&output.stdout).unwrap();
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

    let started_deadline = Instant::now() + Duration::from_secs(2);
    while !starts.exists() && Instant::now() < started_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(&starts).unwrap(), "start\n");
    fs::write(&release, b"").unwrap();

    let result_path = jobs.join(job_id).join("result.json");
    let result_deadline = Instant::now() + Duration::from_secs(2);
    while !result_path.exists() && Instant::now() < result_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(result_path.exists());

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

    let status = Command::new(binary())
        .args(["status", "--thread", "job-flow", "--state-dir"])
        .arg(&state)
        .arg("--json")
        .output()
        .unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
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
    let output = Command::new(binary())
        .args(["run", "--thread", "late-result", "--state-dir"])
        .arg(&state)
        .arg("--job-dir")
        .arg(&jobs)
        .args(["--timeout", "100ms", "--json", "--", "sh", "-c", "exit 7"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let launch: Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = jobs
        .join(launch["job_id"].as_str().unwrap())
        .join("result.json");
    let result_deadline = Instant::now() + Duration::from_secs(2);
    while !result.exists() && Instant::now() < result_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(result.exists());

    let completed = stop_hook(&state, "late-result");
    let reason = completed["reason"].as_str().unwrap();
    assert!(reason.contains("condition met"));
    assert!(reason.contains("exit code 7"));
    assert!(!reason.contains("deadline reached"));
}
