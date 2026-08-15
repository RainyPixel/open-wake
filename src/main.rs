use clap::{Args, Parser, Subcommand, ValueEnum};
use open_wake::doctor::{CheckStatus, DoctorReport, doctor};
use open_wake::job::{self, JobSnapshot};
use open_wake::setup::{ChangeKind, InstallScope, SetupReport, SetupTarget, setup, uninstall};
use open_wake::update::{UpdateReport, check_for_update, install_release};
use open_wake::{
    ArmRequest, Condition, StopHookInput, arm, cancel, current_session_id, default_state_dir,
    handle_stop_hook, hook_config, hook_output, status,
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
    /// Request cancellation of the active condition.
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

    /// Delay between predicate invocations.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    interval: Duration,

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

    /// Also wake periodically without restarting the command.
    #[arg(long, value_parser = parse_duration)]
    check_every: Option<Duration>,

    /// Delay between lightweight job-state checks.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    interval: Duration,

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
    check_every_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    #[serde(flatten)]
    condition: Condition,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_status: Option<JobSnapshot>,
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
                    interval: args.interval,
                    check_timeout: Duration::from_secs(5),
                    check_every: args.check_every,
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
                    job::record_spawn_failure(&job_reference.root, &spec.id, &error);
                    return Err(error);
                }
            };
            let report = RunReport {
                condition_id: condition.id,
                job_id: spec.id,
                supervisor_pid,
                log_path: spec.log_path,
                timeout_ms: args.timeout.as_millis().try_into().unwrap_or(u64::MAX),
                check_every_ms: args
                    .check_every
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
                    "started job {} (supervisor {}); log: {}; finish the Codex turn to wait",
                    report.job_id,
                    report.supervisor_pid,
                    report.log_path.display()
                );
            }
        }
        Commands::Arm(args) => {
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
                    interval: args.interval,
                    check_timeout: args.check_timeout,
                    check_every: None,
                    job: None,
                },
            )?;
            println!(
                "armed condition {} ({}) for {}; finish the Codex turn to wait",
                condition.id,
                condition.label.as_deref().unwrap_or("unnamed"),
                humantime::format_duration(args.timeout)
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
            println!(
                "{}",
                serde_json::to_string(&hook_output(result))
                    .map_err(|error| format!("serialize hook output: {error}"))?
            );
        }
        Commands::Status(args) => {
            let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
            let session_id = current_session_id(args.thread)?;
            let Some(condition) = status(&state_dir, &session_id)? else {
                println!("no condition for Codex session {session_id}");
                return Ok(true);
            };
            let job_snapshot = condition
                .job
                .as_ref()
                .map(|reference| job::snapshot(&reference.root, &reference.id))
                .transpose()?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&StatusReport {
                        condition,
                        job_status: job_snapshot,
                    })
                    .map_err(|error| format!("serialize status: {error}"))?
                );
            } else {
                println!(
                    "{}: {:?}, attempts: {}, last exit: {:?}",
                    condition.label.as_deref().unwrap_or(&condition.id),
                    condition.status,
                    condition.attempts,
                    condition.last_exit_code
                );
                if let Some(job) = job_snapshot {
                    println!("{}", job.summary());
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
                println!("cancellation requested for condition {}", condition.id);
            }
        }
        Commands::Setup(args) => {
            let target = resolve_target(args.scope, args.project_dir.as_deref())?;
            let report = setup(&target, args.dry_run)?;
            print_setup_report(&report, args.json)?;
            if !args.dry_run && !args.json {
                println!(
                    "next: restart Codex if the skill is not visible, then open `/hooks`, review the command, and trust it"
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

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

fn resolve_target(scope: ScopeArg, project_dir: Option<&Path>) -> Result<SetupTarget, String> {
    match scope {
        ScopeArg::Project => Ok(SetupTarget::project(&resolve_project_root(project_dir)?)),
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
