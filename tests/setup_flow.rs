use open_wake::setup::{
    ChangeKind, SetupTarget, inspect_hook, inspect_hook_enabled, inspect_skill, setup, uninstall,
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use toml_edit::{DocumentMut, value};

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
            "open-wake-setup-test-{}-{timestamp}-{sequence}",
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

#[test]
fn project_setup_merges_idempotently_and_uninstall_preserves_other_hooks() {
    let project = TestDir::new();
    let target = SetupTarget::project(project.as_ref());
    fs::create_dir_all(target.hook_path.parent().unwrap()).unwrap();
    let original_handler = json!({
        "type": "command",
        "command": "printf keep-me",
        "timeout": 3,
        "statusMessage": "Existing hook"
    });
    fs::write(
        &target.hook_path,
        serde_json::to_vec_pretty(&json!({
            "description": "existing configuration",
            "custom": true,
            "hooks": {
                "Stop": [{"hooks": [original_handler.clone()]}],
                "Notification": [{"hooks": [{"type": "command", "command": "notify"}]}]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let first = setup(&target, false).unwrap();
    assert!(
        first
            .changes
            .iter()
            .all(|change| { matches!(change.action, ChangeKind::Created | ChangeKind::Updated) })
    );
    inspect_hook(&target).unwrap();
    inspect_skill(&target).unwrap();

    let root: Value = serde_json::from_slice(&fs::read(&target.hook_path).unwrap()).unwrap();
    assert_eq!(root["description"], "existing configuration");
    assert_eq!(root["custom"], true);
    let stop_groups = root["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop_groups.len(), 2);
    assert_eq!(stop_groups[0]["hooks"][0], original_handler);
    assert!(root["hooks"]["Notification"].is_array());
    assert_eq!(
        fs::metadata(&target.hook_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );

    let second = setup(&target, false).unwrap();
    assert!(
        second
            .changes
            .iter()
            .all(|change| change.action == ChangeKind::Unchanged)
    );

    uninstall(&target, false).unwrap();
    let root: Value = serde_json::from_slice(&fs::read(&target.hook_path).unwrap()).unwrap();
    assert_eq!(root["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(root["hooks"]["Stop"][0]["hooks"][0], original_handler);
    assert!(root["hooks"]["Notification"].is_array());
    assert!(!target.skill_dir.exists());
}

#[test]
fn dry_run_writes_nothing_and_modified_skill_is_not_deleted() {
    let project = TestDir::new();
    let target = SetupTarget::project(project.as_ref());

    let report = setup(&target, true).unwrap();
    assert!(
        report
            .changes
            .iter()
            .all(|change| change.action == ChangeKind::Created)
    );
    assert!(!target.hook_path.exists());
    assert!(!target.skill_dir.exists());

    setup(&target, false).unwrap();
    let skill = target.skill_dir.join("SKILL.md");
    fs::write(&skill, "locally modified\n").unwrap();
    let report = uninstall(&target, false).unwrap();
    assert!(
        report.changes.iter().any(|change| {
            change.path == skill && change.action == ChangeKind::RetainedModified
        })
    );
    assert_eq!(fs::read_to_string(skill).unwrap(), "locally modified\n");
}

#[test]
fn user_setup_uses_private_hook_file_and_absolute_binary() {
    let root = TestDir::new();
    let home = root.as_ref().join("home");
    let codex_home = root.as_ref().join("codex-home");
    let binary = root.as_ref().join("bin/open-wake");
    let target = SetupTarget::user(&home, &codex_home, &binary);
    let state_key = format!("{}:stop:0:0", target.hook_path.display());
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(
        codex_home.join("config.toml"),
        format!(
            "# preserve this comment\n[features]\nhooks = false\n\n[hooks.state.\"{state_key}\"]\nenabled = false\ntrusted_hash = \"sha256:keep\"\n\n[hooks.state.\"unrelated:stop:0:0\"]\nenabled = false\n"
        ),
    )
    .unwrap();

    setup(&target, false).unwrap();
    inspect_hook(&target).unwrap();
    assert!(inspect_hook_enabled(&target).unwrap());
    let root: Value = serde_json::from_slice(&fs::read(&target.hook_path).unwrap()).unwrap();
    assert_eq!(
        root["hooks"]["Stop"][0]["hooks"][0]["command"],
        format!("'{}' hook", binary.display())
    );
    assert_eq!(
        fs::metadata(&target.hook_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let config = fs::read_to_string(codex_home.join("config.toml")).unwrap();
    assert!(config.contains("# preserve this comment"));
    let document = config.parse::<DocumentMut>().unwrap();
    assert_eq!(document["features"]["hooks"].as_bool(), Some(true));
    assert_eq!(
        document["hooks"]["state"][&state_key]["enabled"].as_bool(),
        Some(true)
    );
    assert_eq!(
        document["hooks"]["state"][&state_key]["trusted_hash"].as_str(),
        Some("sha256:keep")
    );
    assert_eq!(
        document["hooks"]["state"]["unrelated:stop:0:0"]["enabled"].as_bool(),
        Some(false)
    );

    let mut disabled = document;
    disabled["hooks"]["state"][&state_key]["enabled"] = value(false);
    fs::write(codex_home.join("config.toml"), disabled.to_string()).unwrap();
    assert!(!inspect_hook_enabled(&target).unwrap());

    setup(&target, false).unwrap();
    assert!(inspect_hook_enabled(&target).unwrap());
    uninstall(&target, false).unwrap();
    let config = fs::read_to_string(codex_home.join("config.toml")).unwrap();
    let document = config.parse::<DocumentMut>().unwrap();
    assert!(document["hooks"]["state"].get(&state_key).is_none());
    assert_eq!(
        document["hooks"]["state"]["unrelated:stop:0:0"]["enabled"].as_bool(),
        Some(false)
    );
}

#[test]
fn cli_setup_doctor_and_uninstall_complete_a_project_lifecycle() {
    let root = TestDir::new();
    let project = root.as_ref().join("project");
    let home = root.as_ref().join("home");
    let bin = root.as_ref().join("bin");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();

    let executable = PathBuf::from(env!("CARGO_BIN_EXE_open-wake"));
    symlink(&executable, bin.join("open-wake")).unwrap();
    let fake_codex = bin.join("codex");
    fs::write(
        &fake_codex,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'codex-cli test' ;;\n  features) echo 'hooks stable true' ;;\n  *) exit 2 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let setup_output = Command::new(&executable)
        .args(["setup", "--scope", "project", "--project-dir"])
        .arg(&project)
        .args(["--json"])
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("OPEN_WAKE_NO_UPDATE_CHECK", "1")
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        setup_output.status.success(),
        "{}",
        String::from_utf8_lossy(&setup_output.stderr)
    );

    let doctor_output = Command::new(&executable)
        .args(["doctor", "--scope", "project", "--project-dir"])
        .arg(&project)
        .arg("--json")
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("OPEN_WAKE_NO_UPDATE_CHECK", "1")
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        doctor_output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&doctor_output.stdout),
        String::from_utf8_lossy(&doctor_output.stderr)
    );
    let report: Value = serde_json::from_slice(&doctor_output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| { check["name"] == "project_hook_trust" && check["status"] == "warn" })
    );
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| { check["name"] == "project_hook_enabled" && check["status"] == "pass" })
    );

    let config_path = home.join(".codex/config.toml");
    let state_key = format!("{}:stop:0:0", project.join(".codex/hooks.json").display());
    let mut config = fs::read_to_string(&config_path)
        .unwrap()
        .parse::<DocumentMut>()
        .unwrap();
    config["hooks"]["state"][&state_key]["enabled"] = value(false);
    fs::write(&config_path, config.to_string()).unwrap();

    let disabled_doctor = Command::new(&executable)
        .args(["doctor", "--scope", "project", "--project-dir"])
        .arg(&project)
        .arg("--json")
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("OPEN_WAKE_NO_UPDATE_CHECK", "1")
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(!disabled_doctor.status.success());
    let report: Value = serde_json::from_slice(&disabled_doctor.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| { check["name"] == "project_hook_enabled" && check["status"] == "fail" })
    );

    let repair = Command::new(&executable)
        .args(["setup", "--scope", "project", "--project-dir"])
        .arg(&project)
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(repair.status.success());

    let uninstall_output = Command::new(&executable)
        .args(["uninstall", "--scope", "project", "--project-dir"])
        .arg(&project)
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(uninstall_output.status.success());
    assert!(!project.join(".codex/hooks.json").exists());
    assert!(!project.join(".agents/skills/open-wake").exists());

    let user_setup = Command::new(&executable)
        .args(["setup", "--scope", "user"])
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(user_setup.status.success());
    fs::create_dir_all(project.join(".codex")).unwrap();
    fs::write(
        project.join(".codex/hooks.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"printf unrelated"}]}]}}"#,
    )
    .unwrap();
    let automatic_doctor = Command::new(&executable)
        .args(["doctor", "--project-dir"])
        .arg(&project)
        .arg("--json")
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("OPEN_WAKE_NO_UPDATE_CHECK", "1")
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(automatic_doctor.status.success());
    let report: Value = serde_json::from_slice(&automatic_doctor.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert!(
        !report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| { check["name"] == "project_hook" })
    );
}

#[test]
fn update_check_reports_a_new_release_without_modifying_the_binary() {
    let root = TestDir::new();
    let bin = root.as_ref().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_curl = bin.join("curl");
    fs::write(
        &fake_curl,
        "#!/bin/sh\nprintf '%s\\n' '{\"tag_name\":\"v9.0.0\",\"html_url\":\"https://example.test/v9.0.0\",\"assets\":[]}'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_open-wake"));
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::new(&executable)
        .args(["update", "--check", "--json"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["latest"], "9.0.0");
    assert_eq!(report["update_available"], true);

    let output = Command::new(&executable)
        .arg("update")
        .env("PATH", &path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
}

#[test]
fn doctor_reuses_update_cache_but_explicit_check_is_fresh() {
    let root = TestDir::new();
    let project = root.as_ref().join("project");
    let bin = root.as_ref().join("bin");
    let count = root.as_ref().join("curl-count");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let fake_codex = bin.join("codex");
    fs::write(
        &fake_codex,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'codex-cli test' ;;\n  features) echo 'hooks stable true' ;;\n  *) exit 2 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_curl = bin.join("curl");
    fs::write(
        &fake_curl,
        format!(
            "#!/bin/sh\nprintf x >>'{}'\nprintf '%s\\n' '{{\"tag_name\":\"v9.0.0\",\"html_url\":\"https://example.test/v9.0.0\",\"assets\":[]}}'\n",
            count.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_open-wake"));
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    for _ in 0..2 {
        let output = Command::new(&executable)
            .args(["doctor", "--scope", "project", "--project-dir"])
            .arg(&project)
            .args(["--job-dir"])
            .arg(root.as_ref().join("jobs"))
            .arg("--json")
            .env("HOME", root.as_ref())
            .env("OPEN_WAKE_CACHE_DIR", root.as_ref().join("cache"))
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(serde_json::from_slice::<Value>(&output.stdout).is_ok());
    }
    assert_eq!(fs::read_to_string(&count).unwrap(), "x");

    let output = Command::new(&executable)
        .args(["update", "--check", "--json"])
        .env("OPEN_WAKE_CACHE_DIR", root.as_ref().join("cache"))
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&count).unwrap(), "xx");
}
