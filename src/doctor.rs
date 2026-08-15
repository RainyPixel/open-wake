use crate::setup::{InstallScope, SetupTarget, inspect_hook, inspect_skill};
use crate::update::check_for_update_cached;
use crate::{
    ArmRequest, ConditionStatus, StopHookInput, arm, directory_can_be_created, handle_stop_hook,
    status,
};
use crate::{job, job::JobState};
use serde::Serialize;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT_DOCTOR_DIR: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn new() -> Self {
        Self {
            ok: true,
            checks: Vec::new(),
        }
    }

    pub fn push(&mut self, check: DoctorCheck) {
        if check.status == CheckStatus::Fail {
            self.ok = false;
        }
        self.checks.push(check);
    }
}

impl Default for DoctorReport {
    fn default() -> Self {
        Self::new()
    }
}

pub fn doctor(
    binary: &Path,
    targets: &[SetupTarget],
    state_dir: &Path,
    job_root: &Path,
) -> DoctorReport {
    let mut report = DoctorReport::new();
    inspect_binary(binary, &mut report);
    inspect_codex(&mut report);
    inspect_state_directory(state_dir, &mut report);
    inspect_protocol(&mut report);
    inspect_jobs(job_root, &mut report);
    inspect_release(&mut report);

    if targets.is_empty() {
        report.push(DoctorCheck {
            name: "installation".to_owned(),
            status: CheckStatus::Fail,
            detail: "no project or user installation was found".to_owned(),
            fix: Some(
                "run `open-wake setup --scope project` or `open-wake setup --scope user`"
                    .to_owned(),
            ),
        });
    }

    for target in targets {
        inspect_target(target, &mut report);
    }
    report
}

fn inspect_state_directory(state_dir: &Path, report: &mut DoctorReport) {
    let usable = directory_can_be_created(state_dir);
    report.push(DoctorCheck {
        name: "condition_state".to_owned(),
        status: if usable {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if usable {
            format!("writable runtime state directory: {}", state_dir.display())
        } else {
            format!(
                "runtime state directory is not writable: {}",
                state_dir.display()
            )
        },
        fix: (!usable).then(|| {
            "choose a writable directory with OPEN_WAKE_STATE_DIR or --state-dir".to_owned()
        }),
    });
}

fn inspect_release(report: &mut DoctorReport) {
    if env::var_os("OPEN_WAKE_NO_UPDATE_CHECK").is_some() {
        return;
    }
    match check_for_update_cached() {
        Ok((update, _)) if update.update_available => report.push(DoctorCheck {
            name: "release_update".to_owned(),
            status: CheckStatus::Warn,
            detail: format!(
                "open-wake {} is available; current version is {}{}",
                update.latest,
                update.current,
                if update.cached { " (cached check)" } else { "" }
            ),
            fix: Some("run `open-wake update`".to_owned()),
        }),
        Ok((update, _)) => report.push(DoctorCheck {
            name: "release_update".to_owned(),
            status: CheckStatus::Pass,
            detail: format!(
                "open-wake {} is the latest release{}",
                update.current,
                if update.cached { " (cached check)" } else { "" }
            ),
            fix: None,
        }),
        Err(error) => report.push(DoctorCheck {
            name: "release_update".to_owned(),
            status: CheckStatus::Warn,
            detail: format!("could not check GitHub Releases: {error}"),
            fix: Some(
                "check the network or set `OPEN_WAKE_NO_UPDATE_CHECK=1` for offline doctor runs"
                    .to_owned(),
            ),
        }),
    }
}

fn inspect_jobs(job_root: &Path, report: &mut DoctorReport) {
    if !directory_can_be_created(job_root) {
        report.push(DoctorCheck {
            name: "job_supervisors".to_owned(),
            status: CheckStatus::Fail,
            detail: format!("job directory is not writable: {}", job_root.display()),
            fix: Some("choose a writable directory with OPEN_WAKE_JOB_DIR or --job-dir".to_owned()),
        });
        return;
    }
    match job::list(job_root) {
        Ok(jobs) => {
            let stale = jobs
                .iter()
                .filter(|job| job.state == JobState::Stale)
                .collect::<Vec<_>>();
            if stale.is_empty() {
                let active = jobs
                    .iter()
                    .filter(|job| matches!(job.state, JobState::Starting | JobState::Running))
                    .count();
                report.push(DoctorCheck {
                    name: "job_supervisors".to_owned(),
                    status: CheckStatus::Pass,
                    detail: format!(
                        "{}: {} recorded jobs, {active} active, no stale supervisor heartbeats",
                        job_root.display(),
                        jobs.len(),
                    ),
                    fix: None,
                });
            } else {
                let detail = stale
                    .iter()
                    .map(|job| format!("{} ({})", job.id, job.log_path.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                report.push(DoctorCheck {
                    name: "job_supervisors".to_owned(),
                    status: CheckStatus::Warn,
                    detail: format!(
                        "stale supervisor heartbeat for {}; a child command may still be running",
                        detail
                    ),
                    fix: Some(
                        "inspect the recorded log and process IDs before stopping anything; doctor never kills or deletes jobs"
                            .to_owned(),
                    ),
                });
            }
        }
        Err(error) => report.push(DoctorCheck {
            name: "job_supervisors".to_owned(),
            status: CheckStatus::Warn,
            detail: format!("could not inspect {}: {error}", job_root.display()),
            fix: Some("inspect the job directory permissions and JSON records".to_owned()),
        }),
    }
}

fn inspect_binary(binary: &Path, report: &mut DoctorReport) {
    match fs::metadata(binary) {
        Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => {
            report.push(DoctorCheck {
                name: "executable".to_owned(),
                status: CheckStatus::Pass,
                detail: binary.display().to_string(),
                fix: None,
            });
        }
        Ok(_) => report.push(DoctorCheck {
            name: "executable".to_owned(),
            status: CheckStatus::Fail,
            detail: format!("{} is not an executable file", binary.display()),
            fix: Some("reinstall open-wake with `cargo install --path . --locked`".to_owned()),
        }),
        Err(error) => report.push(DoctorCheck {
            name: "executable".to_owned(),
            status: CheckStatus::Fail,
            detail: format!("inspect {}: {error}", binary.display()),
            fix: Some("reinstall open-wake with `cargo install --path . --locked`".to_owned()),
        }),
    }
}

fn inspect_codex(report: &mut DoctorReport) {
    let version = Command::new("codex").arg("--version").output();
    match version {
        Ok(output) if output.status.success() => report.push(DoctorCheck {
            name: "codex_cli".to_owned(),
            status: CheckStatus::Pass,
            detail: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            fix: None,
        }),
        Ok(output) => report.push(DoctorCheck {
            name: "codex_cli".to_owned(),
            status: CheckStatus::Fail,
            detail: format!("`codex --version` exited with {}", output.status),
            fix: Some("install or repair Codex CLI, then rerun `open-wake doctor`".to_owned()),
        }),
        Err(error) => report.push(DoctorCheck {
            name: "codex_cli".to_owned(),
            status: CheckStatus::Fail,
            detail: format!("could not run `codex --version`: {error}"),
            fix: Some("install Codex CLI and ensure `codex` is on PATH".to_owned()),
        }),
    }

    let features = Command::new("codex").args(["features", "list"]).output();
    match features {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let hooks_enabled = stdout.lines().any(|line| {
                let columns = line.split_whitespace().collect::<Vec<_>>();
                columns.first() == Some(&"hooks") && columns.last() == Some(&"true")
            });
            report.push(DoctorCheck {
                name: "codex_hooks".to_owned(),
                status: if hooks_enabled {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                },
                detail: if hooks_enabled {
                    "Codex reports the hooks feature enabled".to_owned()
                } else {
                    "Codex does not report the hooks feature enabled".to_owned()
                },
                fix: (!hooks_enabled)
                    .then(|| "upgrade Codex CLI to a release with hooks enabled".to_owned()),
            });
        }
        Ok(output) => report.push(DoctorCheck {
            name: "codex_hooks".to_owned(),
            status: CheckStatus::Fail,
            detail: format!("`codex features list` exited with {}", output.status),
            fix: Some("upgrade or repair Codex CLI".to_owned()),
        }),
        Err(error) => report.push(DoctorCheck {
            name: "codex_hooks".to_owned(),
            status: CheckStatus::Fail,
            detail: format!("could not inspect Codex features: {error}"),
            fix: Some("install Codex CLI and ensure `codex` is on PATH".to_owned()),
        }),
    }
}

fn inspect_protocol(report: &mut DoctorReport) {
    match protocol_smoke_test() {
        Ok(()) => report.push(DoctorCheck {
            name: "wake_protocol".to_owned(),
            status: CheckStatus::Pass,
            detail: "arm, Stop hook, and continuation smoke test passed".to_owned(),
            fix: None,
        }),
        Err(error) => report.push(DoctorCheck {
            name: "wake_protocol".to_owned(),
            status: CheckStatus::Fail,
            detail: error,
            fix: Some(
                "rerun with `RUST_BACKTRACE=1 open-wake doctor` and report the output".to_owned(),
            ),
        }),
    }
}

fn inspect_target(target: &SetupTarget, report: &mut DoctorReport) {
    let prefix = target.scope.as_str();
    match inspect_hook(target) {
        Ok(()) => report.push(DoctorCheck {
            name: format!("{prefix}_hook"),
            status: CheckStatus::Pass,
            detail: target.hook_path.display().to_string(),
            fix: None,
        }),
        Err(error) => report.push(DoctorCheck {
            name: format!("{prefix}_hook"),
            status: CheckStatus::Fail,
            detail: error,
            fix: Some(target.setup_command()),
        }),
    }
    match inspect_skill(target) {
        Ok(()) => report.push(DoctorCheck {
            name: format!("{prefix}_skill"),
            status: CheckStatus::Pass,
            detail: target.skill_dir.display().to_string(),
            fix: None,
        }),
        Err(error) => report.push(DoctorCheck {
            name: format!("{prefix}_skill"),
            status: CheckStatus::Fail,
            detail: error,
            fix: Some(target.setup_command()),
        }),
    }

    if target.scope == InstallScope::Project && find_on_path("open-wake").is_none() {
        report.push(DoctorCheck {
            name: "project_hook_command".to_owned(),
            status: CheckStatus::Fail,
            detail: "project hooks use `open-wake hook`, but `open-wake` is not on PATH".to_owned(),
            fix: Some("install it with `cargo install --path . --locked`".to_owned()),
        });
    }

    report.push(DoctorCheck {
        name: format!("{prefix}_hook_trust"),
        status: CheckStatus::Warn,
        detail: "hook trust is interactive Codex state and cannot be verified non-interactively"
            .to_owned(),
        fix: Some("open `/hooks`, review the command, and trust this hook definition".to_owned()),
    });
}

fn protocol_smoke_test() -> Result<(), String> {
    let directory = DoctorDir::new()?;
    let session_id = format!("doctor-{}", std::process::id());
    arm(
        directory.as_ref(),
        ArmRequest {
            session_id: session_id.clone(),
            label: Some("doctor smoke test".to_owned()),
            cwd: directory.as_ref().to_owned(),
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf doctor-ok".to_owned(),
            ],
            timeout: Duration::from_secs(2),
            interval: Duration::from_millis(50),
            check_timeout: Duration::from_secs(1),
            check_every: None,
            job: None,
        },
    )?;
    let result = handle_stop_hook(
        directory.as_ref(),
        &StopHookInput {
            session_id: session_id.clone(),
            hook_event_name: "Stop".to_owned(),
        },
    )?;
    if !matches!(result, crate::HookResult::Continue(ref reason) if reason.contains("doctor-ok")) {
        return Err("Stop hook did not produce the expected continuation".to_owned());
    }
    let condition = status(directory.as_ref(), &session_id)?
        .ok_or_else(|| "doctor condition disappeared".to_owned())?;
    if condition.status != ConditionStatus::Succeeded {
        return Err(format!(
            "doctor condition resolved as {:?}",
            condition.status
        ));
    }
    Ok(())
}

fn find_on_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(command))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

struct DoctorDir(PathBuf);

impl DoctorDir {
    fn new() -> Result<Self, String> {
        let sequence = NEXT_DOCTOR_DIR.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "open-wake-doctor-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)
            .map_err(|error| format!("create doctor workspace {}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl AsRef<Path> for DoctorDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for DoctorDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_jobs_are_warnings_and_are_not_removed() {
        let workspace = DoctorDir::new().unwrap();
        let root = workspace.as_ref().join("jobs");
        let spec = job::prepare(
            &root,
            "thread".to_owned(),
            Some("stale build".to_owned()),
            workspace.as_ref().to_owned(),
            vec!["true".to_owned()],
        )
        .unwrap();
        let spec_path = root.join(&spec.id).join("job.json");
        let mut stored: job::JobSpec =
            serde_json::from_slice(&fs::read(&spec_path).unwrap()).unwrap();
        stored.created_at_ms = 0;
        fs::write(&spec_path, serde_json::to_vec_pretty(&stored).unwrap()).unwrap();

        let mut report = DoctorReport::new();
        inspect_jobs(&root, &mut report);

        let check = report
            .checks
            .iter()
            .find(|check| check.name == "job_supervisors")
            .unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains(&spec.id));
        assert!(spec_path.exists());
        assert!(report.ok);
    }
}
