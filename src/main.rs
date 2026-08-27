use clap::{Args, Parser, Subcommand, ValueEnum};
use open_wake::doctor::{CheckStatus, DoctorReport, doctor};
use open_wake::job::{self, JobSnapshot};
use open_wake::setup::{ChangeKind, InstallScope, SetupReport, SetupTarget, setup, uninstall};
use open_wake::update::{UpdateReport, check_for_update, install_release};
use open_wake::{
    ArmRequest, Condition, DeliveryStatus, HookResult, StopHookInput, arm, cancel,
    cancel_if_current, current_session_id, default_state_dir, deliver_hook_notification,
    handle_stop_hook, hook_config, hook_output, inspect_condition, status,
};
use serde::Serialize;
use std::env;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run one local command under a detached supervisor and wake Codex when it ends.
    Run(RunArgs),
    /// Arm one condition for the current Codex session and return immediately.
    Arm(ArmArgs),
    /// Codex Stop hook entry point. Reads the hook event as JSON from stdin.
    #[command(hide = true)]
    Hook(StateArgs),
    /// Show the condition for a Codex session.
    Status(SessionArgs),
    /// Print the absolute combined stdout/stderr log path for a supervised job.
    Logs(LogsArgs),
    /// Cancel the condition immediately without stopping an attached job.
    Cancel(SessionArgs),
    /// Install or update the Stop hook and Codex skill.
    Setup(InstallArgs),
    /// Check Codex, the wake protocol, hook configuration, and skill installation.
    Doctor(DoctorArgs),
    /// Remove only files and hook entries managed by open-wake.
    Uninstall(InstallArgs),
    /// Check for a new GitHub release and optionally replace this executable.
    Update(UpdateArgs),
    /// Print the Codex hooks.json fragment for this executable.
    HookConfig,
    /// Detached job supervisor entry point.
    #[command(hide = true)]
    Supervise(SuperviseArgs),
    /// Predicate used by run to detect a terminal job state.
    #[command(hide = true)]
    JobReady(JobReadyArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScopeArg {
    Project,
    #[value(alias = "global")]
    User,
}

impl From<ScopeArg> for InstallScope {
    fn from(scope: ScopeArg) -> Self {
        match scope {
            ScopeArg::Project => Self::Project,
            ScopeArg::User => Self::User,
        }
    }
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// Install for this repository, or for the current user (`global` is an alias).
    #[arg(long, value_enum)]
    scope: ScopeArg,

    /// Repository directory for project scope. Defaults to the current repository.
    #[arg(long)]
    project_dir: Option<PathBuf>,

    /// Report changes without writing them.
    #[arg(long)]
    dry_run: bool,

    /// Emit a machine-readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Check one scope. Without this flag, doctor checks every detected installation.
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,

    /// Repository directory for project scope. Defaults to the current repository.
    #[arg(long)]
    project_dir: Option<PathBuf>,

    /// Emit a machine-readable report.
    #[arg(long)]
    json: bool,

    /// Override the persistent supervised-job directory.
    #[arg(long)]
    job_dir: Option<PathBuf>,

    /// Override the runtime condition directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct UpdateArgs {
    /// Only report whether a newer release exists.
    #[arg(long)]
    check: bool,

    /// Install without an interactive confirmation.
    #[arg(long)]
    yes: bool,

    /// Emit a machine-readable check report.
    #[arg(long, requires = "check")]
    json: bool,
}

#[derive(Debug, Args)]
struct ArmArgs {
    /// Human-readable name included in the continuation prompt.
    #[arg(long)]
    label: Option<String>,

    /// Stop waiting and continue Codex when this deadline is reached.
    #[arg(long, default_value = "1h", value_parser = parse_duration)]
    timeout: Duration,

    /// Delay between local predicate invocations. This does not wake Codex.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    poll_every: Duration,

    /// Wake Codex with a progress checkpoint at this interval.
    ///
    /// This creates a model turn; the minimum is 1m.
    #[arg(long, value_parser = parse_duration)]
    checkpoint_every: Option<Duration>,

    /// Maximum duration of one predicate invocation.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    check_timeout: Duration,

    /// Override CODEX_THREAD_ID. Primarily useful for diagnostics.
    #[arg(long)]
    thread: Option<String>,

    /// Override the runtime state directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Read-only, idempotent predicate. Exit 0 means the condition is met.
    #[arg(last = true, required = true)]
    predicate: Vec<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Human-readable job name included in continuation prompts.
    #[arg(long)]
    label: Option<String>,

    /// Wake at this deadline even if the command is still running.
    #[arg(long, default_value = "1h", value_parser = parse_duration)]
    timeout: Duration,

    /// Wake Codex with a progress checkpoint at this interval.
    ///
    /// This creates a model turn; the minimum is 1m.
    #[arg(long, value_parser = parse_duration)]
    checkpoint_every: Option<Duration>,

    /// Override CODEX_THREAD_ID.
    #[arg(long)]
    thread: Option<String>,

    /// Override the runtime condition directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Override the persistent supervised-job directory.
    #[arg(long)]
    job_dir: Option<PathBuf>,

    /// Emit a machine-readable launch report.
    #[arg(long)]
    json: bool,

    /// Command to supervise.
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct StateArgs {
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SessionArgs {
    /// Override CODEX_THREAD_ID.
    #[arg(long)]
    thread: Option<String>,

    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Emit the complete machine-readable condition.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct LogsArgs {
    /// Job ID. Defaults to the job attached to the current session condition.
    job: Option<String>,

    /// Override CODEX_THREAD_ID when JOB is omitted.
    #[arg(long)]
    thread: Option<String>,

    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Override the persistent supervised-job directory when JOB is provided.
    #[arg(long)]
    job_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SuperviseArgs {
    #[arg(long)]
    job_path: PathBuf,
}

#[derive(Debug, Args)]
struct JobReadyArgs {
    #[arg(long)]
    job_dir: PathBuf,

    #[arg(long)]
    job: String,
}

#[derive(Debug, Serialize)]
struct RunReport {
    condition_id: String,
    job_id: String,
    supervisor_pid: u32,
    log_path: PathBuf,
    timeout_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint_every_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    #[serde(flatten)]
    condition: Condition,
    watcher: open_wake::WatcherStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_status: Option<JobSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_error: Option<String>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("open-wake: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    match cli.command {
        Commands::Run(args) => {
            warn_frequent_checkpoint(args.checkpoint_every);
            let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
            let job_root = args.job_dir.unwrap_or_else(job::default_job_root);
            let session_id = current_session_id(args.thread)?;
            let cwd =
                env::current_dir().map_err(|error| format!("read current directory: {error}"))?;
            let binary = env::current_exe()
                .map_err(|error| format!("resolve current executable: {error}"))?;
            let spec = job::prepare(
                &job_root,
                session_id.clone(),
                args.label.clone(),
                cwd.clone(),
                args.command,
            )?;
            let job_reference = job::reference(&job_root, &spec)?;
            let predicate = vec![
                binary.display().to_string(),
                "job-ready".to_owned(),
                "--job-dir".to_owned(),
                job_reference.root.display().to_string(),
                "--job".to_owned(),
                spec.id.clone(),
            ];
            let condition = match arm(
                &state_dir,
                ArmRequest {
                    session_id,
                    label: args.label,
                    cwd,
                    command: predicate,
                    timeout: args.timeout,
                    poll_every: Duration::from_secs(1),
                    check_timeout: Duration::from_secs(5),
                    checkpoint_every: args.checkpoint_every,
                    job: Some(job_reference.clone()),
                },
            ) {
                Ok(condition) => condition,
                Err(error) => {
                    job::discard_prepared(&job_root, &spec.id);
                    return Err(error);
                }
            };
            let supervisor_pid = match job::spawn_supervisor(&binary, &job_reference.root, &spec.id)
            {
                Ok(pid) => pid,
                Err(error) => {
                    let mut recovery = Vec::new();
                    match cancel_if_current(&state_dir, &condition.session_id, &condition.id) {
                        Ok(Some(_)) => {
                            recovery.push(format!("cancelled condition {}", condition.id))
                        }
                        Ok(None) => recovery.push(format!(
                            "left replacement condition unchanged after condition {}",
                            condition.id
                        )),
                        Err(cancel_error) => recovery.push(format!(
                            "failed to cancel condition {}: {cancel_error}",
                            condition.id
                        )),
                    }
                    match job::try_record_spawn_failure(&job_reference.root, &spec.id, &error) {
                        Ok(()) => recovery.push(format!("retained failed job record {}", spec.id)),
                        Err(record_error) => recovery.push(format!(
                            "failed to retain job record {}: {record_error}",
                            spec.id
                        )),
                    }
                    return Err(format!("{error}; {}", recovery.join("; ")));
                }
            };
            let report = RunReport {
                condition_id: condition.id,
                job_id: spec.id,
                supervisor_pid,
                log_path: spec.log_path,
                timeout_ms: args.timeout.as_millis().try_into().unwrap_or(u64::MAX),
                checkpoint_every_ms: args
                    .checkpoint_every
                    .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX)),
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .map_err(|error| format!("serialize run report: {error}"))?
                );
            } else {
                println!(
                    "started job {} (supervisor {}); log: {}; {}; finish the Codex turn to wait",
                    report.job_id,
                    report.supervisor_pid,
                    report.log_path.display(),
                    checkpoint_note(args.checkpoint_every)
                );
            }
        }
        Commands::Arm(args) => {
            warn_frequent_checkpoint(args.checkpoint_every);
            let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
            let session_id = current_session_id(args.thread)?;
            let condition = arm(
                &state_dir,
                ArmRequest {
                    session_id,
                    label: args.label,
                    cwd: env::current_dir()
                        .map_err(|error| format!("read current directory: {error}"))?,
                    command: args.predicate,
                    timeout: args.timeout,
                    poll_every: args.poll_every,
                    check_timeout: args.check_timeout,
                    checkpoint_every: args.checkpoint_every,
                    job: None,
                },
            )?;
            println!(
                "armed condition {} ({}) for {}; {}; finish the Codex turn to wait",
                condition.id,
                condition.label.as_deref().unwrap_or("unnamed"),
                humantime::format_duration(args.timeout),
                checkpoint_note(args.checkpoint_every)
            );
        }
        Commands::Hook(args) => {
            let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .map_err(|error| format!("read Stop hook input: {error}"))?;
            let input: StopHookInput = serde_json::from_str(&input)
                .map_err(|error| format!("parse Stop hook input: {error}"))?;
            let result = handle_stop_hook(&state_dir, &input)?;
            let needs_acknowledgement = matches!(result, HookResult::Continue(_));
            let mut output = serde_json::to_vec(&hook_output(result))
                .map_err(|error| format!("serialize hook output: {error}"))?;
            output.push(b'\n');
            if needs_acknowledgement {
                match deliver_hook_notification(&state_dir, &input, || write_hook_stdout(&output))?
                {
                    DeliveryStatus::Delivered => {}
                    DeliveryStatus::Superseded => write_hook_stdout(b"{}\n")?,
                    DeliveryStatus::DeliveredButUnacknowledged(error) => eprintln!(
                        "open-wake: warning: hook output was delivered but its state acknowledgement failed: {error}"
                    ),
                }
            } else {
                write_hook_stdout(&output)?;
            }
        }
        Commands::Status(args) => {
            let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
            let session_id = current_session_id(args.thread)?;
            let Some(snapshot) = inspect_condition(&state_dir, &session_id)? else {
                println!("no condition for Codex session {session_id}");
                return Ok(true);
            };
            let condition = snapshot.condition;
            let (job_snapshot, job_error) = match condition.job.as_ref() {
                Some(reference) => match job::snapshot(&reference.root, &reference.id) {
                    Ok(snapshot) => (Some(snapshot), None),
                    Err(error) => (None, Some(error)),
                },
                None => (None, None),
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&StatusReport {
                        condition,
                        watcher: snapshot.watcher,
                        job_status: job_snapshot,
                        job_error,
                    })
                    .map_err(|error| format!("serialize status: {error}"))?
                );
            } else {
                println!(
                    "{}: {:?}, watcher: {:?}, phase: {:?}, turn: {:?}, attempts: {}, last exit: {:?}",
                    condition.label.as_deref().unwrap_or(&condition.id),
                    condition.status,
                    snapshot.watcher.state,
                    snapshot.watcher.phase,
                    snapshot.watcher.turn_id,
                    condition.attempts,
                    condition.last_exit_code
                );
                if let Some(error) = snapshot.watcher.lease_error.as_ref() {
                    println!("watcher lease: {error}");
                }
                if let Some(job) = job_snapshot {
                    println!("{}", job.summary());
                }
                if let Some(error) = job_error {
                    println!("job status unavailable: {error}");
                }
            }
        }
        Commands::Logs(args) => {
            let (root, id) = if let Some(id) = args.job {
                (args.job_dir.unwrap_or_else(job::default_job_root), id)
            } else {
                let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
                let session_id = current_session_id(args.thread)?;
                let condition = status(&state_dir, &session_id)?
                    .ok_or_else(|| format!("no condition for Codex session {session_id}"))?;
                let reference = condition.job.ok_or_else(|| {
                    format!(
                        "condition {} is not attached to a supervised job",
                        condition.id
                    )
                })?;
                (reference.root, reference.id)
            };
            println!("{}", job::log_path(&root, &id)?.display());
        }
        Commands::Cancel(args) => {
            let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
            let session_id = current_session_id(args.thread)?;
            let condition = cancel(&state_dir, &session_id)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&condition)
                        .map_err(|error| format!("serialize condition: {error}"))?
                );
            } else {
                println!(
                    "cancelled condition {}; any supervised job remains unchanged",
                    condition.id
                );
            }
        }
        Commands::Setup(args) => {
            let target = resolve_target(args.scope, args.project_dir.as_deref())?;
            let report = setup(&target, args.dry_run)?;
            print_setup_report(&report, args.json)?;
            if !args.dry_run && !args.json {
                println!(
                    "next: restart Codex, then open `/hooks`, verify the hook is enabled, review the command, and trust it if requested"
                );
            }
        }
        Commands::Doctor(args) => {
            let binary = env::current_exe()
                .map_err(|error| format!("resolve current executable: {error}"))?;
            let targets = match args.scope {
                Some(scope) => vec![resolve_target(scope, args.project_dir.as_deref())?],
                None => {
                    let candidates = [
                        resolve_target(ScopeArg::Project, args.project_dir.as_deref())?,
                        resolve_target(ScopeArg::User, None)?,
                    ];
                    candidates
                        .into_iter()
                        .filter(SetupTarget::has_any_installation)
                        .collect()
                }
            };
            let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
            let job_root = args.job_dir.unwrap_or_else(job::default_job_root);
            let report = doctor(&binary, &targets, &state_dir, &job_root);
            print_doctor_report(&report, args.json)?;
            return Ok(report.ok);
        }
        Commands::Uninstall(args) => {
            let target = resolve_target(args.scope, args.project_dir.as_deref())?;
            let report = uninstall(&target, args.dry_run)?;
            print_setup_report(&report, args.json)?;
            return Ok(!report
                .changes
                .iter()
                .any(|change| change.action == ChangeKind::RetainedModified));
        }
        Commands::Update(args) => {
            let (report, release) = check_for_update()?;
            print_update_report(&report, args.json)?;
            if args.check || !report.update_available {
                return Ok(true);
            }
            if !args.yes {
                if !io::stdin().is_terminal() {
                    return Err(
                        "an update is available; rerun `open-wake update --yes` to install it"
                            .to_owned(),
                    );
                }
                print!("Install open-wake {} now? [y/N] ", report.latest);
                io::stdout()
                    .flush()
                    .map_err(|error| format!("flush update prompt: {error}"))?;
                let mut answer = String::new();
                io::stdin()
                    .read_line(&mut answer)
                    .map_err(|error| format!("read update confirmation: {error}"))?;
                if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
                    println!("update cancelled");
                    return Ok(true);
                }
            }
            let executable = env::current_exe()
                .map_err(|error| format!("resolve current executable: {error}"))?;
            install_release(&release, &executable)?;
            println!(
                "updated open-wake to {}; run `open-wake doctor` and refresh each installed scope with `open-wake setup --scope ...`",
                report.latest
            );
        }
        Commands::HookConfig => {
            let binary = env::current_exe()
                .map_err(|error| format!("resolve current executable: {error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&hook_config(&binary))
                    .map_err(|error| format!("serialize hook config: {error}"))?
            );
        }
        Commands::Supervise(args) => {
            job::supervise(&args.job_path)?;
        }
        Commands::JobReady(args) => {
            let snapshot = job::snapshot(&args.job_dir, &args.job)?;
            println!("{}", snapshot.summary());
            return Ok(snapshot.is_ready());
        }
    }
    Ok(true)
}

fn write_hook_stdout(output: &[u8]) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(output)
        .map_err(|error| format!("write hook output: {error}"))?;
    stdout
        .flush()
        .map_err(|error| format!("flush hook output: {error}"))
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

fn warn_frequent_checkpoint(checkpoint_every: Option<Duration>) {
    if checkpoint_every.is_some_and(|interval| interval < Duration::from_secs(5 * 60)) {
        eprintln!(
            "open-wake: warning: --checkpoint-every creates a model turn every interval; prefer 5m or longer"
        );
    }
}

fn checkpoint_note(checkpoint_every: Option<Duration>) -> String {
    checkpoint_every.map_or_else(
        || "terminal-only wakeups".to_owned(),
        |interval| {
            format!(
                "checkpoint every {} creates model turns",
                humantime::format_duration(interval)
            )
        },
    )
}

fn resolve_target(scope: ScopeArg, project_dir: Option<&Path>) -> Result<SetupTarget, String> {
    match scope {
        ScopeArg::Project => {
            let target = SetupTarget::project(&resolve_project_root(project_dir)?);
            Ok(match resolve_codex_config_path() {
                Some(path) => target.with_codex_config_path(path),
                None => target,
            })
        }
        ScopeArg::User => {
            let home = env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| "HOME is not set; user scope cannot be resolved".to_owned())?;
            let codex_home = env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            let binary = env::current_exe()
                .map_err(|error| format!("resolve current executable: {error}"))?;
            Ok(SetupTarget::user(&home, &codex_home, &binary))
        }
    }
}

fn resolve_codex_config_path() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .map(|codex_home| codex_home.join("config.toml"))
}

fn resolve_project_root(explicit: Option<&Path>) -> Result<PathBuf, String> {
    let directory = explicit
        .map(Path::to_owned)
        .map(Ok)
        .unwrap_or_else(env::current_dir)
        .map_err(|error| format!("resolve project directory: {error}"))?;
    let directory = directory
        .canonicalize()
        .map_err(|error| format!("resolve {}: {error}", directory.display()))?;
    let output = Command::new("git")
        .args(["-C"])
        .arg(&directory)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !root.is_empty() {
            return Ok(PathBuf::from(root));
        }
    }
    Ok(directory)
}

fn print_setup_report(report: &SetupReport, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report)
                .map_err(|error| format!("serialize setup report: {error}"))?
        );
        return Ok(());
    }
    println!(
        "{} scope{}:",
        report.scope.as_str(),
        if report.dry_run { " (dry run)" } else { "" }
    );
    for change in &report.changes {
        let action = match change.action {
            ChangeKind::Created => "CREATE",
            ChangeKind::Updated => "UPDATE",
            ChangeKind::Unchanged => "OK",
            ChangeKind::Removed => "REMOVE",
            ChangeKind::Missing => "MISSING",
            ChangeKind::RetainedModified => "KEEP MODIFIED",
        };
        println!("  {action:13} {}", change.path.display());
    }
    Ok(())
}

fn print_doctor_report(report: &DoctorReport, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report)
                .map_err(|error| format!("serialize doctor report: {error}"))?
        );
        return Ok(());
    }
    for check in &report.checks {
        let status = match check.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        println!("{status:4} {:24} {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("     fix: {fix}");
        }
    }
    println!(
        "doctor: {}",
        if report.ok {
            "healthy (warnings may require interactive review)"
        } else {
            "problems found"
        }
    );
    Ok(())
}

fn print_update_report(report: &UpdateReport, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report)
                .map_err(|error| format!("serialize update report: {error}"))?
        );
    } else if report.update_available {
        println!(
            "update available: {} -> {} ({})",
            report.current, report.latest, report.release_url
        );
    } else {
        println!(
            "open-wake {} is current (latest release: {})",
            report.current, report.latest
        );
    }
    Ok(())
}
