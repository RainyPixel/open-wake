use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod doctor;
pub mod job;
pub mod setup;
pub(crate) mod state;
pub mod update;

pub const HOOK_TIMEOUT_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const MAX_CONDITION_TIMEOUT: Duration = Duration::from_secs(HOOK_TIMEOUT_SECONDS - 60);
const OUTPUT_LIMIT_BYTES: usize = 4 * 1024;
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MIN_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);
const HOOK_LEASE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const HOOK_LEASE_STALE_AFTER: Duration = Duration::from_secs(5);
const LEGACY_HOOK_GRACE_MARGIN: Duration = Duration::from_secs(5);
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
    // Kept for source and record compatibility. New code never writes this
    // transitional state; it is terminal and `cancel` normalizes it.
    #[doc(hidden)]
    CancelRequested,
    Succeeded,
    TimedOut,
    Failed,
    Cancelled,
}

impl ConditionStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Armed | Self::Waiting)
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
    #[serde(default, alias = "interval_ms")]
    pub poll_every_ms: u64,
    pub check_timeout_ms: u64,
    #[serde(
        default,
        alias = "check_every_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub checkpoint_every_ms: Option<u64>,
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
    pub poll_every: Duration,
    pub check_timeout: Duration,
    pub checkpoint_every: Option<Duration>,
    pub job: Option<JobReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HookLease {
    version: u8,
    condition_id: String,
    owner_id: String,
    pid: u32,
    heartbeat_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatcherState {
    Alive,
    Stale,
    LegacyUnknown,
    Armed,
    Inactive,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatcherStatus {
    pub state: WatcherState,
    pub condition_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_error: Option<String>,
}

#[derive(Debug)]
pub struct ConditionSnapshot {
    pub condition: Condition,
    pub watcher: WatcherStatus,
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
    let _lock = lock_condition(state_dir, &request.session_id)?;
    let path = condition_path(state_dir, &request.session_id);

    match read_condition(&path) {
        Ok(existing) if existing.status.is_active() => {
            return Err(format!(
                "condition {} is already active for this Codex session (status: {:?})",
                existing.id, existing.status
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("read existing condition: {error}")),
    }

    let created_at_ms = now_ms();
    let checkpoint_every_ms = request.checkpoint_every.map(duration_ms);
    let condition = Condition {
        version: 2,
        id: new_condition_id(),
        session_id: request.session_id,
        label: request.label,
        cwd: request.cwd,
        command: request.command,
        created_at_ms,
        timeout_ms: duration_ms(request.timeout),
        poll_every_ms: duration_ms(request.poll_every),
        check_timeout_ms: duration_ms(request.check_timeout),
        checkpoint_every_ms,
        next_checkpoint_at_ms: checkpoint_every_ms.map(|interval| created_at_ms + interval),
        checkpoints: 0,
        job: request.job,
        status: ConditionStatus::Armed,
        attempts: 0,
        last_exit_code: None,
        last_output: String::new(),
        resolved_at_ms: None,
    };
    write_condition(&path, &condition).map_err(io_error("write condition"))?;
    let _ = fs::remove_file(hook_lease_path(
        path.parent().unwrap_or(Path::new(".")),
        &condition.session_id,
    ));
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

pub fn inspect_condition(
    state_dir: &Path,
    session_id: &str,
) -> Result<Option<ConditionSnapshot>, String> {
    let path = condition_path(state_dir, session_id);
    if !path.exists() {
        return Ok(None);
    }
    let _lock = lock_condition(state_dir, session_id)?;
    match read_condition(&path) {
        Ok(condition) => Ok(Some(ConditionSnapshot {
            watcher: watcher_status(&path, &condition, now_ms()),
            condition,
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read condition: {error}")),
    }
}

pub fn cancel(state_dir: &Path, session_id: &str) -> Result<Condition, String> {
    let _lock = lock_condition(state_dir, session_id)?;
    let path = condition_path(state_dir, session_id);
    let condition = read_condition(&path).map_err(io_error("read condition"))?;
    cancel_condition(&path, condition)
}

#[doc(hidden)]
pub fn cancel_if_current(
    state_dir: &Path,
    session_id: &str,
    expected_id: &str,
) -> Result<Option<Condition>, String> {
    let _lock = lock_condition(state_dir, session_id)?;
    let path = condition_path(state_dir, session_id);
    let condition = read_condition(&path).map_err(io_error("read condition"))?;
    if condition.id != expected_id {
        return Ok(None);
    }
    cancel_condition(&path, condition).map(Some)
}

fn cancel_condition(path: &Path, mut condition: Condition) -> Result<Condition, String> {
    if matches!(
        condition.status,
        ConditionStatus::CancelRequested | ConditionStatus::Cancelled
    ) {
        if condition.status != ConditionStatus::Cancelled || condition.resolved_at_ms.is_none() {
            mark_resolved(&mut condition, ConditionStatus::Cancelled);
            write_condition(path, &condition).map_err(io_error("resolve legacy cancellation"))?;
        }
        return Ok(condition);
    }
    if !condition.status.is_active() {
        return Err(format!(
            "condition {} is already terminal (status: {:?}); inspect its recorded outcome before arming another condition and do not use cancel as a launch prefix",
            condition.id, condition.status
        ));
    }
    mark_resolved(&mut condition, ConditionStatus::Cancelled);
    write_condition(path, &condition).map_err(io_error("cancel condition"))?;
    let _ = fs::remove_file(hook_lease_path(
        path.parent().unwrap_or(Path::new(".")),
        &condition.session_id,
    ));
    Ok(condition)
}

pub fn handle_stop_hook(state_dir: &Path, input: &StopHookInput) -> Result<HookResult, String> {
    if input.hook_event_name != "Stop" {
        return Ok(HookResult::Noop);
    }

    let (mut condition, owner_id) = {
        let _lock = lock_condition(state_dir, &input.session_id)?;
        let path = condition_path(state_dir, &input.session_id);
        let mut condition = match read_condition(&path) {
            Ok(condition)
                if condition.status == ConditionStatus::Armed
                    || condition.status == ConditionStatus::Waiting =>
            {
                condition
            }
            Ok(_) => return Ok(HookResult::Noop),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(HookResult::Noop);
            }
            Err(error) => return Err(format!("read condition: {error}")),
        };
        if condition.status == ConditionStatus::Waiting
            && watcher_status(&path, &condition, now_ms()).state != WatcherState::Stale
        {
            // A fresh lease already has an owner, and a legacy v1 record may
            // still be owned by a hook from the previous binary version.
            return Ok(HookResult::Noop);
        }
        let owner_id = new_condition_id();
        migrate_condition_to_v2(&mut condition);
        condition.status = ConditionStatus::Waiting;
        write_condition(&path, &condition).map_err(io_error("mark condition waiting"))?;
        if let Ok(old_lease) = read_hook_lease(&hook_lease_path(state_dir, &input.session_id)) {
            let _ = fs::remove_file(condition_check_output_path(
                state_dir,
                &condition,
                &old_lease.owner_id,
            ));
        }
        let lease = HookLease {
            version: 1,
            condition_id: condition.id.clone(),
            owner_id: owner_id.clone(),
            pid: std::process::id(),
            heartbeat_at_ms: now_ms(),
        };
        write_hook_lease(&hook_lease_path(state_dir, &input.session_id), &lease)
            .map_err(io_error("acquire hook lease"))?;
        (condition, owner_id)
    };
    let _heartbeat_guard = spawn_hook_heartbeat(
        state_dir.to_owned(),
        input.session_id.clone(),
        condition.id.clone(),
        owner_id.clone(),
    );
    let condition_id = condition.id.clone();

    loop {
        let Some(current) = load_waiting_condition(
            state_dir,
            &input.session_id,
            &condition_id,
            &owner_id,
            "refresh condition",
        )?
        else {
            return Ok(HookResult::Noop);
        };
        condition = current;

        if now_ms().saturating_sub(condition.created_at_ms) >= condition.timeout_ms {
            let completed_job = condition
                .job
                .as_ref()
                .and_then(|reference| job::snapshot(&reference.root, &reference.id).ok())
                .filter(|snapshot| snapshot.is_ready())
                .map(|snapshot| format!("{}\n", snapshot.summary()));
            let Some(resolved) = update_waiting_condition(
                state_dir,
                &input.session_id,
                &condition_id,
                &owner_id,
                "resolve condition deadline",
                |current| {
                    if let Some(summary) = &completed_job {
                        current.attempts += 1;
                        current.last_exit_code = Some(0);
                        current.last_output.clone_from(summary);
                        mark_resolved(current, ConditionStatus::Succeeded);
                    } else {
                        mark_resolved(current, ConditionStatus::TimedOut);
                    }
                },
            )?
            else {
                return Ok(HookResult::Noop);
            };
            return Ok(HookResult::Continue(format_continuation(&resolved)));
        }

        let remaining = Duration::from_millis(
            condition
                .timeout_ms
                .saturating_sub(now_ms().saturating_sub(condition.created_at_ms)),
        );
        let check = match run_check(state_dir, &condition, &owner_id, remaining) {
            Ok(check) => check,
            Err(error) => {
                let Some(failed) = update_waiting_condition(
                    state_dir,
                    &input.session_id,
                    &condition_id,
                    &owner_id,
                    "record condition failure",
                    |current| {
                        current.attempts += 1;
                        current.last_output.clone_from(&error);
                        mark_resolved(current, ConditionStatus::Failed);
                    },
                )?
                else {
                    return Ok(HookResult::Noop);
                };
                return Ok(HookResult::Continue(format_continuation(&failed)));
            }
        };
        let observed_at_ms = now_ms();
        let Some(updated) = update_waiting_condition(
            state_dir,
            &input.session_id,
            &condition_id,
            &owner_id,
            "record condition check",
            |current| {
                current.attempts += 1;
                current.last_exit_code = check.exit_code;
                current.last_output.clone_from(&check.output);
                if check.exit_code == Some(0) && !check.timed_out {
                    mark_resolved(current, ConditionStatus::Succeeded);
                } else if current
                    .next_checkpoint_at_ms
                    .is_some_and(|checkpoint_at_ms| observed_at_ms >= checkpoint_at_ms)
                {
                    current.status = ConditionStatus::Armed;
                    current.checkpoints += 1;
                    current.next_checkpoint_at_ms = current
                        .checkpoint_every_ms
                        .map(|interval| observed_at_ms.saturating_add(interval));
                }
            },
        )?
        else {
            return Ok(HookResult::Noop);
        };
        condition = updated;

        if condition.status == ConditionStatus::Succeeded {
            return Ok(HookResult::Continue(format_continuation(&condition)));
        }
        if condition.status == ConditionStatus::Armed {
            return Ok(HookResult::Continue(format_checkpoint(&condition)));
        }

        let remaining_ms = condition
            .timeout_ms
            .saturating_sub(now_ms().saturating_sub(condition.created_at_ms));
        let checkpoint_ms = condition
            .next_checkpoint_at_ms
            .map(|checkpoint| checkpoint.saturating_sub(now_ms()))
            .unwrap_or(u64::MAX);
        let sleep_for =
            Duration::from_millis(condition.poll_every_ms.min(remaining_ms).min(checkpoint_ms));
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
    if request.poll_every < MIN_POLL_INTERVAL {
        return Err("--poll-every must be at least 50ms".to_owned());
    }
    if request.check_timeout.is_zero() {
        return Err("--check-timeout must be greater than zero".to_owned());
    }
    if let Some(checkpoint_every) = request.checkpoint_every {
        if checkpoint_every < MIN_CHECKPOINT_INTERVAL {
            return Err("--checkpoint-every must be at least 1m".to_owned());
        }
        if checkpoint_every >= request.timeout {
            return Err("--checkpoint-every must be shorter than --timeout".to_owned());
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
    owner_id: &str,
    remaining: Duration,
) -> Result<CheckResult, String> {
    let output_path = condition_check_output_path(state_dir, condition, owner_id);
    let result = run_check_with_output(condition, remaining, &output_path);
    let _ = fs::remove_file(output_path);
    result
}

fn run_check_with_output(
    condition: &Condition,
    remaining: Duration,
    output_path: &Path,
) -> Result<CheckResult, String> {
    let output = open_private_truncate(output_path).map_err(io_error("open check output"))?;
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
        output: read_tail(output_path, OUTPUT_LIMIT_BYTES)
            .map_err(io_error("read check output"))?,
        timed_out,
    })
}

fn mark_resolved(condition: &mut Condition, status: ConditionStatus) {
    condition.status = status;
    condition.resolved_at_ms = Some(now_ms());
}

fn migrate_condition_to_v2(condition: &mut Condition) {
    let observed_at_ms = now_ms();
    let remaining_timeout_ms = condition
        .timeout_ms
        .saturating_sub(observed_at_ms.saturating_sub(condition.created_at_ms));
    let legacy_checkpoint_every_ms = condition.checkpoint_every_ms;
    let migrated_checkpoint_every_ms = legacy_checkpoint_every_ms
        .map(|interval_ms| interval_ms.max(duration_ms(MIN_CHECKPOINT_INTERVAL)));
    let migrated_checkpoint_every_ms =
        migrated_checkpoint_every_ms.filter(|interval_ms| *interval_ms < remaining_timeout_ms);

    condition.version = 2;
    let preserve_next_checkpoint = legacy_checkpoint_every_ms.is_some()
        && legacy_checkpoint_every_ms == migrated_checkpoint_every_ms
        && condition.next_checkpoint_at_ms.is_some();
    condition.checkpoint_every_ms = migrated_checkpoint_every_ms;
    if !preserve_next_checkpoint {
        condition.next_checkpoint_at_ms = migrated_checkpoint_every_ms
            .map(|interval_ms| observed_at_ms.saturating_add(interval_ms));
    }
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

fn condition_lock_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join(format!(".{}.lock", safe_name(session_id)))
}

fn condition_check_output_path(state_dir: &Path, condition: &Condition, owner_id: &str) -> PathBuf {
    state_dir.join(format!(
        "{}.{}.{}.last-check.log",
        safe_name(&condition.session_id),
        safe_name(&condition.id),
        safe_name(owner_id)
    ))
}

fn hook_lease_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join(format!(".{}.owner.json", safe_name(session_id)))
}

fn watcher_status(
    condition_path: &Path,
    condition: &Condition,
    observed_at_ms: u64,
) -> WatcherStatus {
    if condition.status == ConditionStatus::Armed {
        return WatcherStatus {
            state: WatcherState::Armed,
            condition_id: condition.id.clone(),
            owner_id: None,
            pid: None,
            heartbeat_at_ms: None,
            heartbeat_age_ms: None,
            lease_error: None,
        };
    }
    if !condition.status.is_active() {
        return WatcherStatus {
            state: WatcherState::Inactive,
            condition_id: condition.id.clone(),
            owner_id: None,
            pid: None,
            heartbeat_at_ms: None,
            heartbeat_age_ms: None,
            lease_error: None,
        };
    }

    let lease_path = hook_lease_path(
        condition_path.parent().unwrap_or_else(|| Path::new(".")),
        &condition.session_id,
    );
    match read_hook_lease(&lease_path) {
        Ok(lease) => {
            let heartbeat_age_ms = observed_at_ms.saturating_sub(lease.heartbeat_at_ms);
            let matches_condition = lease.condition_id == condition.id;
            let future_heartbeat = lease.heartbeat_at_ms > observed_at_ms.saturating_add(1_000);
            let process_alive = process_exists(lease.pid);
            let alive = matches_condition
                && !future_heartbeat
                && process_alive
                && heartbeat_age_ms <= duration_ms(HOOK_LEASE_STALE_AFTER);
            let lease_error = if future_heartbeat {
                Some("hook lease heartbeat is in the future".to_owned())
            } else if !process_alive {
                Some("hook process no longer exists".to_owned())
            } else {
                None
            };
            WatcherStatus {
                state: if alive {
                    WatcherState::Alive
                } else {
                    WatcherState::Stale
                },
                condition_id: condition.id.clone(),
                owner_id: Some(lease.owner_id),
                pid: Some(lease.pid),
                heartbeat_at_ms: Some(lease.heartbeat_at_ms),
                heartbeat_age_ms: Some(heartbeat_age_ms),
                lease_error,
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let legacy_recent = condition.version == 1
                && !legacy_condition_is_stale(condition_path, condition, observed_at_ms);
            WatcherStatus {
                state: if legacy_recent {
                    WatcherState::LegacyUnknown
                } else {
                    WatcherState::Stale
                },
                condition_id: condition.id.clone(),
                owner_id: None,
                pid: None,
                heartbeat_at_ms: None,
                heartbeat_age_ms: None,
                lease_error: None,
            }
        }
        Err(error) => WatcherStatus {
            state: WatcherState::Stale,
            condition_id: condition.id.clone(),
            owner_id: None,
            pid: None,
            heartbeat_at_ms: None,
            heartbeat_age_ms: None,
            lease_error: Some(error.to_string()),
        },
    }
}

fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn legacy_condition_is_stale(
    condition_path: &Path,
    condition: &Condition,
    observed_at_ms: u64,
) -> bool {
    let maximum_update_gap = Duration::from_millis(
        condition
            .poll_every_ms
            .saturating_add(condition.check_timeout_ms),
    ) + LEGACY_HOOK_GRACE_MARGIN;
    let grace_ms = duration_ms(maximum_update_gap.max(Duration::from_secs(30)));
    match fs::metadata(condition_path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => observed_at_ms.saturating_sub(system_time_ms(modified)) >= grace_ms,
        Err(_) => true,
    }
}

fn lock_condition(state_dir: &Path, session_id: &str) -> Result<File, String> {
    state::ensure_state_dir(state_dir).map_err(io_error("create state directory"))?;
    let lock_path = condition_lock_path(state_dir, session_id);
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(io_error("open condition lock"))?;
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(lock);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("lock condition: {error}"));
        }
    }
}

// Atomic rename keeps individual records readable. The session lock makes the
// read/validate/write transition linearizable across CLI and hook processes.
fn read_waiting_condition(
    path: &Path,
    expected_id: &str,
    expected_owner_id: &str,
    context: &str,
) -> Result<Option<Condition>, String> {
    match read_condition(path) {
        Ok(condition)
            if condition.id == expected_id && condition.status == ConditionStatus::Waiting =>
        {
            let lease_path = hook_lease_path(
                path.parent().unwrap_or_else(|| Path::new(".")),
                &condition.session_id,
            );
            Ok(
                if hook_lease_is_owned(&lease_path, expected_id, expected_owner_id)? {
                    Some(condition)
                } else {
                    None
                },
            )
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{context}: {error}")),
    }
}

fn load_waiting_condition(
    state_dir: &Path,
    session_id: &str,
    expected_id: &str,
    expected_owner_id: &str,
    context: &'static str,
) -> Result<Option<Condition>, String> {
    let _lock = lock_condition(state_dir, session_id)?;
    let path = condition_path(state_dir, session_id);
    read_waiting_condition(&path, expected_id, expected_owner_id, context)
}

fn update_waiting_condition(
    state_dir: &Path,
    session_id: &str,
    expected_id: &str,
    expected_owner_id: &str,
    context: &'static str,
    update: impl FnOnce(&mut Condition),
) -> Result<Option<Condition>, String> {
    let _lock = lock_condition(state_dir, session_id)?;
    let path = condition_path(state_dir, session_id);
    let Some(mut condition) =
        read_waiting_condition(&path, expected_id, expected_owner_id, context)?
    else {
        return Ok(None);
    };
    let mut lease = read_hook_lease(&hook_lease_path(state_dir, session_id))
        .map_err(|error| format!("{context}: read hook lease: {error}"))?;
    update(&mut condition);
    write_condition(&path, &condition).map_err(|error| format!("{context}: {error}"))?;
    if condition.status.is_active() {
        lease.heartbeat_at_ms = now_ms();
        write_hook_lease(&hook_lease_path(state_dir, session_id), &lease)
            .map_err(|error| format!("{context}: refresh hook lease: {error}"))?;
    } else {
        let _ = fs::remove_file(hook_lease_path(state_dir, session_id));
    }
    Ok(Some(condition))
}

fn hook_lease_is_owned(
    lease_path: &Path,
    condition_id: &str,
    owner_id: &str,
) -> Result<bool, String> {
    match read_hook_lease(lease_path) {
        Ok(lease) => Ok(lease.condition_id == condition_id && lease.owner_id == owner_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("read hook lease: {error}")),
    }
}

fn read_hook_lease(path: &Path) -> io::Result<HookLease> {
    let file = File::open(path)?;
    let lease: HookLease = serde_json::from_reader(file)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    validate_hook_lease(&lease)?;
    Ok(lease)
}

fn write_hook_lease(path: &Path, lease: &HookLease) -> io::Result<()> {
    validate_hook_lease(lease)?;
    state::write_private_json(path, lease, "owner")
}

fn validate_hook_lease(lease: &HookLease) -> io::Result<()> {
    if lease.version != 1
        || lease.pid == 0
        || lease.pid > i32::MAX as u32
        || lease.condition_id.is_empty()
        || lease.owner_id.is_empty()
    {
        return Err(io::Error::other("invalid hook owner lease"));
    }
    Ok(())
}

fn spawn_hook_heartbeat(
    state_dir: PathBuf,
    session_id: String,
    condition_id: String,
    owner_id: String,
) -> HeartbeatGuard {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    thread::spawn(move || {
        loop {
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(HOOK_LEASE_REFRESH_INTERVAL);
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            if !refresh_hook_heartbeat(&state_dir, &session_id, &condition_id, &owner_id) {
                break;
            }
        }
    });
    HeartbeatGuard { stop }
}

struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn refresh_hook_heartbeat(
    state_dir: &Path,
    session_id: &str,
    condition_id: &str,
    owner_id: &str,
) -> bool {
    let Ok(_lock) = lock_condition(state_dir, session_id) else {
        return false;
    };
    let path = condition_path(state_dir, session_id);
    let Ok(condition) = read_condition(&path) else {
        return false;
    };
    if condition.id != condition_id || condition.status != ConditionStatus::Waiting {
        return false;
    }
    let lease_path = hook_lease_path(state_dir, session_id);
    let Ok(mut lease) = read_hook_lease(&lease_path) else {
        return false;
    };
    if lease.condition_id != condition_id || lease.owner_id != owner_id {
        return false;
    }
    lease.heartbeat_at_ms = now_ms();
    write_hook_lease(&lease_path, &lease).is_ok()
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
    let condition: Condition = serde_json::from_reader(file)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    validate_condition(&condition)?;
    Ok(condition)
}

fn write_condition(path: &Path, condition: &Condition) -> io::Result<()> {
    validate_condition(condition)?;
    state::write_private_json(path, condition, "condition")
}

fn validate_condition(condition: &Condition) -> io::Result<()> {
    if !matches!(condition.version, 1 | 2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported condition version {}", condition.version),
        ));
    }
    if condition.timeout_ms == 0 {
        return Err(io::Error::other(
            "condition timeout must be greater than zero",
        ));
    }
    if condition.poll_every_ms < duration_ms(MIN_POLL_INTERVAL) {
        return Err(io::Error::other("condition poll interval is too short"));
    }
    if condition.check_timeout_ms == 0 {
        return Err(io::Error::other(
            "condition check timeout must be greater than zero",
        ));
    }
    if condition.version == 2
        && let Some(checkpoint_every_ms) = condition.checkpoint_every_ms
        && (checkpoint_every_ms < duration_ms(MIN_CHECKPOINT_INTERVAL)
            || checkpoint_every_ms >= condition.timeout_ms)
    {
        return Err(io::Error::other("invalid condition checkpoint interval"));
    }
    if condition.version == 2
        && condition.checkpoint_every_ms.is_some() != condition.next_checkpoint_at_ms.is_some()
    {
        return Err(io::Error::other("inconsistent condition checkpoint state"));
    }
    if condition.version == 2
        && condition
            .next_checkpoint_at_ms
            .is_some_and(|checkpoint_at_ms| {
                checkpoint_at_ms > condition.created_at_ms.saturating_add(condition.timeout_ms)
            })
    {
        return Err(io::Error::other(
            "condition checkpoint is after its deadline",
        ));
    }
    Ok(())
}

fn open_private_truncate(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
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

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
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
