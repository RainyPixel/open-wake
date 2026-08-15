use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod doctor;
pub mod job;
pub mod setup;
pub mod update;

pub const HOOK_TIMEOUT_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const MAX_CONDITION_TIMEOUT: Duration = Duration::from_secs(HOOK_TIMEOUT_SECONDS - 60);
const OUTPUT_LIMIT_BYTES: usize = 4 * 1024;
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MIN_CHECKPOINT_INTERVAL: Duration = Duration::from_millis(100);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobReference {
    pub id: String,
    pub root: PathBuf,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConditionStatus {
    Armed,
    Waiting,
    CancelRequested,
    Succeeded,
    TimedOut,
    Failed,
    Cancelled,
}

impl ConditionStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Armed | Self::Waiting | Self::CancelRequested)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    pub version: u8,
    pub id: String,
    pub session_id: String,
    pub label: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub created_at_ms: u64,
    pub timeout_ms: u64,
    pub interval_ms: u64,
    pub check_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_every_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_checkpoint_at_ms: Option<u64>,
    #[serde(default)]
    pub checkpoints: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobReference>,
    pub status: ConditionStatus,
    pub attempts: u64,
    pub last_exit_code: Option<i32>,
    pub last_output: String,
    pub resolved_at_ms: Option<u64>,
}

#[derive(Debug)]
pub struct ArmRequest {
    pub session_id: String,
    pub label: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub timeout: Duration,
    pub interval: Duration,
    pub check_timeout: Duration,
    pub check_every: Option<Duration>,
    pub job: Option<JobReference>,
}

#[derive(Debug, Deserialize)]
pub struct StopHookInput {
    pub session_id: String,
    #[serde(default)]
    pub hook_event_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HookResult {
    Noop,
    Continue(String),
}

#[derive(Debug)]
struct CheckResult {
    exit_code: Option<i32>,
    output: String,
    timed_out: bool,
}

pub fn default_state_dir() -> PathBuf {
    if let Some(path) = env::var_os("OPEN_WAKE_STATE_DIR") {
        return PathBuf::from(path);
    }
    let preferred = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("open-wake"));
    let fallback = PathBuf::from("/tmp").join(format!("open-wake-{}", current_user_name()));
    choose_writable_directory(preferred, fallback)
}

fn choose_writable_directory(preferred: Option<PathBuf>, fallback: PathBuf) -> PathBuf {
    preferred
        .filter(|path| directory_can_be_created(path))
        .unwrap_or(fallback)
}

pub(crate) fn directory_can_be_created(path: &Path) -> bool {
    let Some(existing) = path.ancestors().find(|ancestor| ancestor.exists()) else {
        return false;
    };
    if !existing.is_dir() {
        return false;
    }
    let Ok(existing) = CString::new(existing.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(existing.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

pub fn current_session_id(explicit: Option<String>) -> Result<String, String> {
    explicit
        .or_else(|| env::var("CODEX_THREAD_ID").ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "CODEX_THREAD_ID is not set; run this command from a Codex turn or pass --thread"
                .to_owned()
        })
}

pub fn arm(state_dir: &Path, request: ArmRequest) -> Result<Condition, String> {
    validate_request(&request)?;
    ensure_state_dir(state_dir).map_err(io_error("create state directory"))?;
    let path = condition_path(state_dir, &request.session_id);

    if let Ok(existing) = read_condition(&path)
        && existing.status.is_active()
    {
        return Err(format!(
            "condition {} is already active for this Codex session (status: {:?})",
            existing.id, existing.status
        ));
    }

    let created_at_ms = now_ms();
    let check_every_ms = request.check_every.map(duration_ms);
    let condition = Condition {
        version: 1,
        id: new_condition_id(),
        session_id: request.session_id,
        label: request.label,
        cwd: request.cwd,
        command: request.command,
        created_at_ms,
        timeout_ms: duration_ms(request.timeout),
        interval_ms: duration_ms(request.interval),
        check_timeout_ms: duration_ms(request.check_timeout),
        check_every_ms,
        next_checkpoint_at_ms: check_every_ms.map(|interval| created_at_ms + interval),
        checkpoints: 0,
        job: request.job,
        status: ConditionStatus::Armed,
        attempts: 0,
        last_exit_code: None,
        last_output: String::new(),
        resolved_at_ms: None,
    };
    write_condition(&path, &condition).map_err(io_error("write condition"))?;
    Ok(condition)
}

pub fn status(state_dir: &Path, session_id: &str) -> Result<Option<Condition>, String> {
    let path = condition_path(state_dir, session_id);
    match read_condition(&path) {
        Ok(condition) => Ok(Some(condition)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read condition: {error}")),
    }
}

pub fn cancel(state_dir: &Path, session_id: &str) -> Result<Condition, String> {
    let path = condition_path(state_dir, session_id);
    let mut condition = read_condition(&path).map_err(io_error("read condition"))?;
    if !condition.status.is_active() {
        return Err(format!(
            "condition {} is not active (status: {:?})",
            condition.id, condition.status
        ));
    }
    condition.status = ConditionStatus::CancelRequested;
    write_condition(&path, &condition).map_err(io_error("request cancellation"))?;
    Ok(condition)
}

pub fn handle_stop_hook(state_dir: &Path, input: &StopHookInput) -> Result<HookResult, String> {
    if input.hook_event_name != "Stop" {
        return Ok(HookResult::Noop);
    }

    let path = condition_path(state_dir, &input.session_id);
    let mut condition = match read_condition(&path) {
        Ok(condition) if condition.status.is_active() => condition,
        Ok(_) => return Ok(HookResult::Noop),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HookResult::Noop),
        Err(error) => return Err(format!("read condition: {error}")),
    };
    let condition_id = condition.id.clone();

    if condition.status == ConditionStatus::CancelRequested {
        resolve(&path, &mut condition, ConditionStatus::Cancelled)?;
        return Ok(HookResult::Noop);
    }

    condition.status = ConditionStatus::Waiting;
    write_condition(&path, &condition).map_err(io_error("mark condition waiting"))?;

    loop {
        if now_ms().saturating_sub(condition.created_at_ms) >= condition.timeout_ms {
            if let Some(reference) = &condition.job
                && let Ok(snapshot) = job::snapshot(&reference.root, &reference.id)
                && snapshot.is_ready()
            {
                condition.attempts += 1;
                condition.last_exit_code = Some(0);
                condition.last_output = format!("{}\n", snapshot.summary());
                resolve(&path, &mut condition, ConditionStatus::Succeeded)?;
                return Ok(HookResult::Continue(format_continuation(&condition)));
            }
            resolve(&path, &mut condition, ConditionStatus::TimedOut)?;
            return Ok(HookResult::Continue(format_continuation(&condition)));
        }

        match read_condition(&path) {
            Ok(current) if current.id != condition_id => return Ok(HookResult::Noop),
            Ok(current) if current.status == ConditionStatus::CancelRequested => {
                condition = current;
                resolve(&path, &mut condition, ConditionStatus::Cancelled)?;
                return Ok(HookResult::Noop);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(HookResult::Noop);
            }
            Err(error) => return Err(format!("refresh condition: {error}")),
        }

        let remaining = Duration::from_millis(
            condition
                .timeout_ms
                .saturating_sub(now_ms().saturating_sub(condition.created_at_ms)),
        );
        let check = match run_check(state_dir, &condition, remaining) {
            Ok(check) => check,
            Err(error) => {
                condition.attempts += 1;
                condition.last_output = error;
                resolve(&path, &mut condition, ConditionStatus::Failed)?;
                return Ok(HookResult::Continue(format_continuation(&condition)));
            }
        };
        condition.attempts += 1;
        condition.last_exit_code = check.exit_code;
        condition.last_output = check.output;

        if check.exit_code == Some(0) && !check.timed_out {
            resolve(&path, &mut condition, ConditionStatus::Succeeded)?;
            return Ok(HookResult::Continue(format_continuation(&condition)));
        }

        let observed_at_ms = now_ms();
        if condition
            .next_checkpoint_at_ms
            .is_some_and(|checkpoint_at_ms| observed_at_ms >= checkpoint_at_ms)
        {
            condition.status = ConditionStatus::Armed;
            condition.checkpoints += 1;
            condition.next_checkpoint_at_ms = condition
                .check_every_ms
                .map(|interval| observed_at_ms.saturating_add(interval));
            write_condition(&path, &condition).map_err(io_error("record checkpoint"))?;
            return Ok(HookResult::Continue(format_checkpoint(&condition)));
        }

        write_condition(&path, &condition).map_err(io_error("update condition"))?;
        let remaining_ms = condition
            .timeout_ms
            .saturating_sub(now_ms().saturating_sub(condition.created_at_ms));
        let checkpoint_ms = condition
            .next_checkpoint_at_ms
            .map(|checkpoint| checkpoint.saturating_sub(now_ms()))
            .unwrap_or(u64::MAX);
        let sleep_for =
            Duration::from_millis(condition.interval_ms.min(remaining_ms).min(checkpoint_ms));
        if !sleep_for.is_zero() {
            thread::sleep(sleep_for);
        }
    }
}

pub fn hook_output(result: HookResult) -> Value {
    match result {
        HookResult::Noop => json!({}),
        HookResult::Continue(reason) => json!({
            "decision": "block",
            "reason": reason,
        }),
    }
}

pub fn hook_config(binary: &Path) -> Value {
    let command = format!("{} hook", shell_quote(binary.as_os_str()));
    setup::managed_hook_config(&command)
}

fn validate_request(request: &ArmRequest) -> Result<(), String> {
    if request.command.is_empty() {
        return Err("predicate command is required after --".to_owned());
    }
    if request.timeout.is_zero() {
        return Err("--timeout must be greater than zero".to_owned());
    }
    if request.timeout > MAX_CONDITION_TIMEOUT {
        return Err(format!(
            "--timeout must be shorter than the configured seven-day Stop hook timeout (max {}s)",
            MAX_CONDITION_TIMEOUT.as_secs()
        ));
    }
    if request.interval < MIN_POLL_INTERVAL {
        return Err("--interval must be at least 50ms".to_owned());
    }
    if request.check_timeout.is_zero() {
        return Err("--check-timeout must be greater than zero".to_owned());
    }
    if let Some(check_every) = request.check_every {
        if check_every < MIN_CHECKPOINT_INTERVAL {
            return Err("--check-every must be at least 100ms".to_owned());
        }
        if check_every >= request.timeout {
            return Err("--check-every must be shorter than --timeout".to_owned());
        }
    }
    if !request.cwd.is_dir() {
        return Err(format!(
            "condition working directory does not exist: {}",
            request.cwd.display()
        ));
    }
    Ok(())
}

fn run_check(
    state_dir: &Path,
    condition: &Condition,
    remaining: Duration,
) -> Result<CheckResult, String> {
    let output_path = state_dir.join(format!(
        "{}.last-check.log",
        safe_name(&condition.session_id)
    ));
    let output = open_private_truncate(&output_path).map_err(io_error("open check output"))?;
    let error_output = output.try_clone().map_err(io_error("clone check output"))?;
    let mut child = Command::new(&condition.command[0])
        .args(&condition.command[1..])
        .current_dir(&condition.cwd)
        .env("OPEN_WAKE_CONDITION_ID", &condition.id)
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error_output))
        .process_group(0)
        .spawn()
        .map_err(|error| format!("start predicate {:?}: {error}", condition.command))?;

    let started = std::time::Instant::now();
    let check_timeout = Duration::from_millis(condition.check_timeout_ms).min(remaining);
    let (exit_code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if started.elapsed() < check_timeout => {
                thread::sleep(MIN_POLL_INTERVAL);
            }
            Ok(None) => {
                // The predicate may have children. Its dedicated process group lets a timed-out
                // check terminate the whole tree instead of leaking background descendants.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let status = child.wait().map_err(io_error("reap timed-out predicate"))?;
                break (status.code(), true);
            }
            Err(error) => return Err(format!("wait for predicate: {error}")),
        }
    };

    Ok(CheckResult {
        exit_code,
        output: read_tail(&output_path, OUTPUT_LIMIT_BYTES)
            .map_err(io_error("read check output"))?,
        timed_out,
    })
}

fn resolve(path: &Path, condition: &mut Condition, status: ConditionStatus) -> Result<(), String> {
    condition.status = status;
    condition.resolved_at_ms = Some(now_ms());
    write_condition(path, condition).map_err(io_error("resolve condition"))
}

fn format_continuation(condition: &Condition) -> String {
    let label = condition_label(condition);
    let elapsed_ms = condition
        .resolved_at_ms
        .unwrap_or_else(now_ms)
        .saturating_sub(condition.created_at_ms);
    let command = condition
        .command
        .iter()
        .map(|argument| shell_quote(OsStr::new(argument)))
        .collect::<Vec<_>>()
        .join(" ");
    let exit_code = condition
        .last_exit_code
        .map_or_else(|| "unavailable".to_owned(), |code| code.to_string());
    let outcome = match condition.status {
        ConditionStatus::Succeeded => "condition met",
        ConditionStatus::TimedOut => "deadline reached before the condition became true",
        ConditionStatus::Failed => "condition check failed",
        _ => "condition resolved",
    };
    let mut message = format!(
        "open-wake: {outcome} for `{label}` after {} ({} checks). Continue the task now. Predicate: `{command}`. Last exit code: {exit_code}.",
        humantime::format_duration(Duration::from_millis(elapsed_ms)),
        condition.attempts,
    );
    append_job_context(&mut message, condition);
    if !condition.last_output.trim().is_empty() {
        message.push_str("\n\nLast predicate output (bounded):\n```text\n");
        message.push_str(condition.last_output.trim_end());
        message.push_str("\n```");
    }
    message
}

fn format_checkpoint(condition: &Condition) -> String {
    let label = condition_label(condition);
    let mut message = format!(
        "open-wake: progress checkpoint {} for `{label}` after {} ({} checks). The same condition remains armed; inspect progress now, then finish the turn to keep waiting or run `open-wake cancel` to stop future wake-ups.",
        condition.checkpoints,
        humantime::format_duration(Duration::from_millis(
            now_ms().saturating_sub(condition.created_at_ms)
        )),
        condition.attempts,
    );
    append_job_context(&mut message, condition);
    if !condition.last_output.trim().is_empty() {
        message.push_str("\n\nLast predicate output (bounded):\n```text\n");
        message.push_str(condition.last_output.trim_end());
        message.push_str("\n```");
    }
    message
}

fn condition_label(condition: &Condition) -> &str {
    condition
        .label
        .as_deref()
        .or_else(|| condition.job.as_ref().map(|job| job.id.as_str()))
        .unwrap_or(&condition.id)
}

fn append_job_context(message: &mut String, condition: &Condition) {
    if let Some(job) = &condition.job {
        message.push_str(&format!(
            " Job ID: `{}`. Full combined stdout/stderr log: `{}`. Use native tools such as `tail` or `rg` to inspect it; open-wake will not print the full log.",
            job.id,
            job.log_path.display()
        ));
        if condition.status == ConditionStatus::TimedOut {
            message.push_str(
                " The open-wake deadline does not terminate the supervised command; inspect the job before deciding what to stop.",
            );
        }
    }
}

fn condition_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join(format!("{}.json", safe_name(session_id)))
}

fn safe_name(value: &str) -> String {
    let mut name = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.') {
            name.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut name, "_{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    name
}

fn read_condition(path: &Path) -> io::Result<Condition> {
    let file = File::open(path)?;
    serde_json::from_reader(file).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_condition(path: &Path, condition: &Condition) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("condition path has no parent"))?;
    ensure_state_dir(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("condition"),
        std::process::id()
    ));
    let mut file = open_private_truncate(&temporary)?;
    serde_json::to_writer_pretty(&mut file, condition).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, path)
}

fn open_private_truncate(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

fn ensure_state_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn read_tail(path: &Path, max_bytes: usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    let start = bytes.len().saturating_sub(max_bytes);
    let mut output = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if truncated {
        output.insert_str(0, "... output truncated ...\n");
    }
    Ok(output)
}

pub(crate) fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn new_condition_id() -> String {
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{sequence}", now_ms(), std::process::id())
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

    #[test]
    fn safe_name_cannot_escape_state_directory() {
        assert_eq!(safe_name("../../thread/one"), ".._2F.._2Fthread_2Fone");
        assert_ne!(safe_name("thread/one"), safe_name("thread_2Fone"));
    }

    #[test]
    fn hook_output_requests_a_continuation_only_for_a_result() {
        assert_eq!(hook_output(HookResult::Noop), json!({}));
        assert_eq!(
            hook_output(HookResult::Continue("ready".to_owned())),
            json!({"decision": "block", "reason": "ready"})
        );
    }

    #[test]
    fn unavailable_runtime_directory_uses_the_writable_fallback() {
        let root = env::temp_dir().join(format!("open-wake-state-choice-{}", new_condition_id()));
        fs::create_dir(&root).unwrap();
        let unavailable = root.join("not-a-directory");
        fs::write(&unavailable, b"").unwrap();
        let fallback = root.join("fallback");

        assert_eq!(
            choose_writable_directory(Some(unavailable.join("open-wake")), fallback.clone()),
            fallback
        );

        fs::remove_dir_all(root).unwrap();
    }
}
