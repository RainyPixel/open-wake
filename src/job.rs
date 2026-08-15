use crate::JobReference;
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const CHILD_CHECK_INTERVAL: Duration = Duration::from_millis(250);
pub const STALE_AFTER: Duration = Duration::from_secs(30);
static NEXT_JOB: AtomicU64 = AtomicU64::new(0);
static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub version: u8,
    pub id: String,
    pub session_id: String,
    pub label: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub created_at_ms: u64,
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub started_at_ms: Option<u64>,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub version: u8,
    pub completed_at_ms: u64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Starting,
    Running,
    Completed,
    SupervisorFailed,
    Stale,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobSnapshot {
    pub id: String,
    pub label: Option<String>,
    pub state: JobState,
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub heartbeat_at_ms: Option<u64>,
    pub result: Option<JobResult>,
    pub log_path: PathBuf,
}

impl JobSnapshot {
    pub fn is_ready(&self) -> bool {
        matches!(
            self.state,
            JobState::Completed | JobState::SupervisorFailed | JobState::Stale
        )
    }

    pub fn summary(&self) -> String {
        let label = self.label.as_deref().unwrap_or(&self.id);
        match self.state {
            JobState::Starting => format!(
                "job `{label}` is starting; log: {}",
                self.log_path.display()
            ),
            JobState::Running => format!(
                "job `{label}` is still running; log: {}",
                self.log_path.display()
            ),
            JobState::Completed => {
                let result = self.result.as_ref().expect("completed job has a result");
                let outcome = if let Some(code) = result.exit_code {
                    format!("exit code {code}")
                } else if let Some(signal) = result.signal {
                    format!("signal {signal}")
                } else {
                    "unknown process status".to_owned()
                };
                format!(
                    "job `{label}` completed with {outcome} after {}; log: {}",
                    humantime::format_duration(Duration::from_millis(result.duration_ms)),
                    self.log_path.display()
                )
            }
            JobState::SupervisorFailed => {
                let error = self
                    .result
                    .as_ref()
                    .and_then(|result| result.error.as_deref())
                    .unwrap_or("unknown supervisor error");
                format!(
                    "job `{label}` supervisor failed: {error}; log: {}",
                    self.log_path.display()
                )
            }
            JobState::Stale => format!(
                "job `{label}` has a stale supervisor heartbeat; the command may still be running; log: {}",
                self.log_path.display()
            ),
        }
    }
}

pub fn default_job_root() -> PathBuf {
    if let Some(path) = env::var_os("OPEN_WAKE_JOB_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("open-wake/jobs");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/open-wake/jobs");
    }
    PathBuf::from("/var/tmp")
        .join(format!("open-wake-{}", current_user_name()))
        .join("jobs")
}

pub fn prepare(
    root: &Path,
    session_id: String,
    label: Option<String>,
    cwd: PathBuf,
    command: Vec<String>,
) -> Result<JobSpec, String> {
    if command.is_empty() {
        return Err("job command is required after --".to_owned());
    }
    if !cwd.is_dir() {
        return Err(format!(
            "job working directory does not exist: {}",
            cwd.display()
        ));
    }
    ensure_private_dir(root).map_err(io_error("create job root"))?;
    let root = root
        .canonicalize()
        .map_err(|error| format!("resolve job root {}: {error}", root.display()))?;
    let id = new_job_id();
    let directory = root.join(&id);
    fs::create_dir(&directory)
        .map_err(|error| format!("create job directory {}: {error}", directory.display()))?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure job directory {}: {error}", directory.display()))?;
    let spec = JobSpec {
        version: 1,
        id,
        session_id,
        label,
        cwd,
        command,
        created_at_ms: now_ms(),
        supervisor_pid: None,
        child_pid: None,
        started_at_ms: None,
        log_path: directory.join("output.log"),
    };
    write_json(&directory.join("job.json"), &spec).map_err(io_error("write job specification"))?;
    Ok(spec)
}

pub fn reference(root: &Path, spec: &JobSpec) -> Result<JobReference, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("resolve job root {}: {error}", root.display()))?;
    if !spec.log_path.starts_with(&root) {
        return Err(format!(
            "job log {} is outside job root {}",
            spec.log_path.display(),
            root.display()
        ));
    }
    Ok(JobReference {
        id: spec.id.clone(),
        root,
        log_path: spec.log_path.clone(),
    })
}

pub fn discard_prepared(root: &Path, id: &str) {
    let Ok(directory) = checked_job_dir(root, id) else {
        return;
    };
    if directory.join("heartbeat").exists() || directory.join("result.json").exists() {
        return;
    }
    let Ok(spec) = read_json::<JobSpec>(&directory.join("job.json")) else {
        return;
    };
    if spec.supervisor_pid.is_none() && spec.child_pid.is_none() {
        let _ = fs::remove_dir_all(directory);
    }
}

pub fn spawn_supervisor(binary: &Path, root: &Path, id: &str) -> Result<u32, String> {
    let directory = checked_job_dir(root, id)?;
    let mut command = Command::new(binary);
    command
        .arg("supervise")
        .arg("--job-path")
        .arg(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| format!("start detached job supervisor: {error}"))
}

pub fn supervise(directory: &Path) -> Result<(), String> {
    let result = supervise_inner(directory);
    if let Err(error) = &result {
        let _ = record_failure(directory, error);
    }
    result
}

fn supervise_inner(directory: &Path) -> Result<(), String> {
    let spec_path = directory.join("job.json");
    let mut spec: JobSpec = read_json(&spec_path).map_err(io_error("read job specification"))?;
    write_heartbeat(directory).map_err(io_error("write supervisor heartbeat"))?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&spec.log_path)
        .map_err(|error| format!("create job log {}: {error}", spec.log_path.display()))?;
    let error_output = output
        .try_clone()
        .map_err(|error| format!("clone job log handle: {error}"))?;
    let started_at_ms = now_ms();
    let mut child = Command::new(&spec.command[0])
        .args(&spec.command[1..])
        .current_dir(&spec.cwd)
        .env("OPEN_WAKE_JOB_ID", &spec.id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error_output))
        .process_group(0)
        .spawn()
        .map_err(|error| format!("start job command {:?}: {error}", spec.command))?;
    spec.supervisor_pid = Some(std::process::id());
    spec.child_pid = Some(child.id());
    spec.started_at_ms = Some(started_at_ms);
    write_json(&spec_path, &spec).map_err(io_error("update job specification"))?;

    let mut last_heartbeat = std::time::Instant::now();
    loop {
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            write_heartbeat(directory).map_err(io_error("refresh supervisor heartbeat"))?;
            last_heartbeat = std::time::Instant::now();
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let completed_at_ms = now_ms();
                let result = JobResult {
                    version: 1,
                    completed_at_ms,
                    duration_ms: completed_at_ms.saturating_sub(started_at_ms),
                    exit_code: status.code(),
                    signal: status.signal(),
                    error: None,
                };
                write_json(&directory.join("result.json"), &result)
                    .map_err(io_error("write job result"))?;
                return Ok(());
            }
            Ok(None) => thread::sleep(CHILD_CHECK_INTERVAL),
            Err(error) => return Err(format!("observe job command: {error}")),
        }
    }
}

pub fn snapshot(root: &Path, id: &str) -> Result<JobSnapshot, String> {
    snapshot_at(root, id, now_ms())
}

fn snapshot_at(root: &Path, id: &str, observed_at_ms: u64) -> Result<JobSnapshot, String> {
    let directory = checked_job_dir(root, id)?;
    let spec: JobSpec =
        read_json(&directory.join("job.json")).map_err(io_error("read job specification"))?;
    let result = match read_json::<JobResult>(&directory.join("result.json")) {
        Ok(result) => Some(result),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read job result: {error}")),
    };
    let heartbeat_at_ms = match fs::read_to_string(directory.join("heartbeat")) {
        Ok(value) => Some(
            value
                .trim()
                .parse::<u64>()
                .map_err(|error| format!("parse job heartbeat: {error}"))?,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read job heartbeat: {error}")),
    };
    let state = if let Some(result) = &result {
        if result.error.is_some() {
            JobState::SupervisorFailed
        } else {
            JobState::Completed
        }
    } else {
        let last_observed = heartbeat_at_ms.unwrap_or(spec.created_at_ms);
        if observed_at_ms.saturating_sub(last_observed) > duration_ms(STALE_AFTER) {
            JobState::Stale
        } else if heartbeat_at_ms.is_some() {
            JobState::Running
        } else {
            JobState::Starting
        }
    };
    Ok(JobSnapshot {
        id: spec.id,
        label: spec.label,
        state,
        supervisor_pid: spec.supervisor_pid,
        child_pid: spec.child_pid,
        created_at_ms: spec.created_at_ms,
        started_at_ms: spec.started_at_ms,
        heartbeat_at_ms,
        result,
        log_path: spec.log_path,
    })
}

pub fn list(root: &Path) -> Result<Vec<JobSnapshot>, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read job root {}: {error}", root.display())),
    };
    let mut jobs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read job root entry: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        jobs.push(snapshot(root, &id)?);
    }
    jobs.sort_by_key(|job| job.created_at_ms);
    Ok(jobs)
}

pub fn log_path(root: &Path, id: &str) -> Result<PathBuf, String> {
    let directory = checked_job_dir(root, id)?;
    let spec: JobSpec =
        read_json(&directory.join("job.json")).map_err(io_error("read job specification"))?;
    Ok(spec.log_path)
}

pub fn record_spawn_failure(root: &Path, id: &str, error: &str) {
    if let Ok(directory) = checked_job_dir(root, id) {
        let _ = record_failure(&directory, error);
    }
}

fn record_failure(directory: &Path, error: &str) -> io::Result<()> {
    let spec: JobSpec = read_json(&directory.join("job.json"))?;
    let completed_at_ms = now_ms();
    let result = JobResult {
        version: 1,
        completed_at_ms,
        duration_ms: completed_at_ms.saturating_sub(spec.created_at_ms),
        exit_code: None,
        signal: None,
        error: Some(error.to_owned()),
    };
    write_json(&directory.join("result.json"), &result)
}

fn checked_job_dir(root: &Path, id: &str) -> Result<PathBuf, String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(format!("invalid job id `{id}`"));
    }
    Ok(root.join(id))
}

fn write_heartbeat(directory: &Path) -> io::Result<()> {
    write_bytes(
        &directory.join("heartbeat"),
        format!("{}\n", now_ms()).as_bytes(),
    )
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_bytes(path, &bytes)
}

fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("job path has no parent"))?;
    let sequence = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(OsStr::to_str).unwrap_or("job"),
        std::process::id(),
        sequence
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let file = File::open(path)?;
    serde_json::from_reader(file).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn new_job_id() -> String {
    let sequence = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("job-{}-{timestamp}-{sequence}", std::process::id())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn current_user_name() -> String {
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .collect()
}

fn io_error(context: &'static str) -> impl FnOnce(io::Error) -> String {
    move |error| format!("{context}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = env::temp_dir().join(new_job_id());
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_heartbeat_becomes_stale_without_claiming_completion() {
        let root = TestDir::new();
        let spec = prepare(
            &root.0,
            "thread".to_owned(),
            Some("build".to_owned()),
            root.0.clone(),
            vec!["true".to_owned()],
        )
        .unwrap();
        let snapshot = snapshot_at(
            &root.0,
            &spec.id,
            spec.created_at_ms + duration_ms(STALE_AFTER) + 1,
        )
        .unwrap();
        assert_eq!(snapshot.state, JobState::Stale);
        assert!(snapshot.result.is_none());
        assert!(snapshot.summary().contains("may still be running"));
    }

    #[test]
    fn job_ids_cannot_escape_the_job_root() {
        assert!(checked_job_dir(Path::new("/jobs"), "../job").is_err());
        assert!(checked_job_dir(Path::new("/jobs"), "job-123").is_ok());
    }
}
