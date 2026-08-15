use crate::{HOOK_TIMEOUT_SECONDS, shell_quote};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MANAGED_STATUS_MESSAGE: &str = "codex-wake: waiting for armed condition";
const LEGACY_STATUS_MESSAGE: &str = "Waiting for the armed condition";
const HOOK_DESCRIPTION: &str = "Resume Codex when an armed local condition completes.";
const SKILL_MD: &str = include_str!("../skills/codex-wake/SKILL.md");
const OPENAI_YAML: &str = include_str!("../skills/codex-wake/agents/openai.yaml");
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallScope {
    Project,
    User,
}

impl InstallScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetupTarget {
    pub scope: InstallScope,
    pub hook_path: PathBuf,
    pub skill_dir: PathBuf,
    pub hook_command: String,
    pub hook_mode: u32,
}

impl SetupTarget {
    pub fn project(project_root: &Path) -> Self {
        Self {
            scope: InstallScope::Project,
            hook_path: project_root.join(".codex/hooks.json"),
            skill_dir: project_root.join(".agents/skills/codex-wake"),
            hook_command: "codex-wake hook".to_owned(),
            hook_mode: 0o644,
        }
    }

    pub fn user(home: &Path, codex_home: &Path, binary: &Path) -> Self {
        Self {
            scope: InstallScope::User,
            hook_path: codex_home.join("hooks.json"),
            skill_dir: home.join(".agents/skills/codex-wake"),
            hook_command: format!("{} hook", shell_quote(binary.as_os_str())),
            hook_mode: 0o600,
        }
    }

    pub fn setup_command(&self) -> String {
        format!("codex-wake setup --scope {}", self.scope.as_str())
    }

    pub fn has_any_installation(&self) -> bool {
        if self.skill_dir.exists() {
            return true;
        }
        fs::read(&self.hook_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|root| managed_handlers(&root).is_ok_and(|handlers| !handlers.is_empty()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Updated,
    Unchanged,
    Removed,
    Missing,
    RetainedModified,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub action: ChangeKind,
}

#[derive(Debug, Serialize)]
pub struct SetupReport {
    pub scope: InstallScope,
    pub dry_run: bool,
    pub changes: Vec<FileChange>,
}

pub fn managed_hook_config(command: &str) -> Value {
    json!({
        "description": HOOK_DESCRIPTION,
        "hooks": {
            "Stop": [managed_hook_group(command)]
        }
    })
}

pub fn setup(target: &SetupTarget, dry_run: bool) -> Result<SetupReport, String> {
    let mut changes = Vec::new();
    let (mut root, existed) = read_hook_root(&target.hook_path)?;
    merge_managed_hook(&mut root, &target.hook_command)?;
    let hook_bytes = pretty_json(&root)?;
    changes.push(install_file(
        &target.hook_path,
        &hook_bytes,
        target.hook_mode,
        dry_run,
        existed,
    )?);
    changes.push(install_file(
        &target.skill_dir.join("SKILL.md"),
        SKILL_MD.as_bytes(),
        0o644,
        dry_run,
        target.skill_dir.join("SKILL.md").exists(),
    )?);
    changes.push(install_file(
        &target.skill_dir.join("agents/openai.yaml"),
        OPENAI_YAML.as_bytes(),
        0o644,
        dry_run,
        target.skill_dir.join("agents/openai.yaml").exists(),
    )?);

    Ok(SetupReport {
        scope: target.scope,
        dry_run,
        changes,
    })
}

pub fn uninstall(target: &SetupTarget, dry_run: bool) -> Result<SetupReport, String> {
    let mut changes = Vec::new();

    if target.hook_path.exists() {
        let (mut root, _) = read_hook_root(&target.hook_path)?;
        let removed = remove_managed_hooks(&mut root)?;
        if removed {
            if is_empty_managed_hook_file(&root) {
                if !dry_run {
                    fs::remove_file(&target.hook_path).map_err(|error| {
                        format!("remove {}: {error}", target.hook_path.display())
                    })?;
                }
                changes.push(FileChange {
                    path: target.hook_path.clone(),
                    action: ChangeKind::Removed,
                });
            } else {
                let bytes = pretty_json(&root)?;
                changes.push(install_file(
                    &target.hook_path,
                    &bytes,
                    target.hook_mode,
                    dry_run,
                    true,
                )?);
            }
        } else {
            changes.push(FileChange {
                path: target.hook_path.clone(),
                action: ChangeKind::Missing,
            });
        }
    } else {
        changes.push(FileChange {
            path: target.hook_path.clone(),
            action: ChangeKind::Missing,
        });
    }

    changes.push(remove_owned_file(
        &target.skill_dir.join("SKILL.md"),
        SKILL_MD.as_bytes(),
        dry_run,
    )?);
    changes.push(remove_owned_file(
        &target.skill_dir.join("agents/openai.yaml"),
        OPENAI_YAML.as_bytes(),
        dry_run,
    )?);

    if !dry_run {
        remove_if_empty(&target.skill_dir.join("agents"))?;
        remove_if_empty(&target.skill_dir)?;
        if let Some(parent) = target.hook_path.parent() {
            remove_if_empty(parent)?;
        }
    }

    Ok(SetupReport {
        scope: target.scope,
        dry_run,
        changes,
    })
}

pub fn inspect_hook(target: &SetupTarget) -> Result<(), String> {
    let (root, _) = read_hook_root(&target.hook_path)?;
    let handlers = managed_handlers(&root)?;
    if handlers.is_empty() {
        return Err(format!(
            "managed Stop hook is missing from {}",
            target.hook_path.display()
        ));
    }
    if handlers.len() != 1 {
        return Err(format!(
            "expected one managed Stop hook in {}, found {}",
            target.hook_path.display(),
            handlers.len()
        ));
    }
    let handler = handlers[0];
    if handler.get("statusMessage").and_then(Value::as_str) != Some(MANAGED_STATUS_MESSAGE) {
        return Err(format!(
            "managed Stop hook marker is stale in {}",
            target.hook_path.display()
        ));
    }
    if handler.get("command").and_then(Value::as_str) != Some(&target.hook_command) {
        return Err(format!(
            "managed Stop hook command is stale in {}",
            target.hook_path.display()
        ));
    }
    if handler.get("timeout").and_then(Value::as_u64) != Some(HOOK_TIMEOUT_SECONDS) {
        return Err(format!(
            "managed Stop hook timeout is stale in {}",
            target.hook_path.display()
        ));
    }
    Ok(())
}

pub fn inspect_skill(target: &SetupTarget) -> Result<(), String> {
    inspect_file(&target.skill_dir.join("SKILL.md"), SKILL_MD.as_bytes())?;
    inspect_file(
        &target.skill_dir.join("agents/openai.yaml"),
        OPENAI_YAML.as_bytes(),
    )
}

fn managed_hook_group(command: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": HOOK_TIMEOUT_SECONDS,
            "statusMessage": MANAGED_STATUS_MESSAGE
        }]
    })
}

fn read_hook_root(path: &Path) -> Result<(Value, bool), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "refusing to replace symlinked hook file {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((json!({ "hooks": {} }), false));
        }
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    }

    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if !root.is_object() {
        return Err(format!("{} must contain a JSON object", path.display()));
    }
    Ok((root, true))
}

fn merge_managed_hook(root: &mut Value, command: &str) -> Result<(), String> {
    remove_managed_hooks(root)?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| "hooks.json root must be an object".to_owned())?;
    object
        .entry("description")
        .or_insert_with(|| Value::String(HOOK_DESCRIPTION.to_owned()));
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| "hooks.json `hooks` must be an object".to_owned())?;
    let stop = hooks
        .entry("Stop")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| "hooks.json `hooks.Stop` must be an array".to_owned())?;
    stop.push(managed_hook_group(command));
    Ok(())
}

fn remove_managed_hooks(root: &mut Value) -> Result<bool, String> {
    let Some(object) = root.as_object_mut() else {
        return Err("hooks.json root must be an object".to_owned());
    };
    let Some(hooks_value) = object.get_mut("hooks") else {
        return Ok(false);
    };
    let hooks = hooks_value
        .as_object_mut()
        .ok_or_else(|| "hooks.json `hooks` must be an object".to_owned())?;
    let Some(stop_value) = hooks.get_mut("Stop") else {
        return Ok(false);
    };
    let stop = stop_value
        .as_array_mut()
        .ok_or_else(|| "hooks.json `hooks.Stop` must be an array".to_owned())?;
    let mut removed = false;
    stop.retain_mut(|group| {
        let Some(group_object) = group.as_object_mut() else {
            return true;
        };
        let Some(group_hooks) = group_object.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = group_hooks.len();
        group_hooks.retain(|handler| !is_managed_handler(handler));
        let group_changed = before != group_hooks.len();
        removed |= group_changed;
        !(group_changed && group_hooks.is_empty())
    });
    if stop.is_empty() {
        hooks.remove("Stop");
    }
    Ok(removed)
}

fn managed_handlers(root: &Value) -> Result<Vec<&Map<String, Value>>, String> {
    let Some(object) = root.as_object() else {
        return Err("hooks.json root must be an object".to_owned());
    };
    let Some(hooks) = object.get("hooks") else {
        return Ok(Vec::new());
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| "hooks.json `hooks` must be an object".to_owned())?;
    let Some(stop) = hooks.get("Stop") else {
        return Ok(Vec::new());
    };
    let stop = stop
        .as_array()
        .ok_or_else(|| "hooks.json `hooks.Stop` must be an array".to_owned())?;
    let mut handlers = Vec::new();
    for group in stop {
        let Some(group_hooks) = group
            .as_object()
            .and_then(|object| object.get("hooks"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for handler in group_hooks {
            if is_managed_handler(handler)
                && let Some(handler) = handler.as_object()
            {
                handlers.push(handler);
            }
        }
    }
    Ok(handlers)
}

fn is_managed_handler(value: &Value) -> bool {
    match value.get("statusMessage").and_then(Value::as_str) {
        Some(MANAGED_STATUS_MESSAGE) => true,
        Some(LEGACY_STATUS_MESSAGE) => {
            value.get("type").and_then(Value::as_str) == Some("command")
                && value
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains("codex-wake"))
        }
        _ => false,
    }
}

fn is_empty_managed_hook_file(root: &Value) -> bool {
    let Some(object) = root.as_object() else {
        return false;
    };
    object.iter().all(|(key, value)| match key.as_str() {
        "description" => value.as_str() == Some(HOOK_DESCRIPTION),
        "hooks" => value.as_object().is_some_and(Map::is_empty),
        _ => false,
    })
}

fn install_file(
    path: &Path,
    expected: &[u8],
    mode: u32,
    dry_run: bool,
    existed: bool,
) -> Result<FileChange, String> {
    if existed {
        let current =
            fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
        if current == expected {
            return Ok(FileChange {
                path: path.to_owned(),
                action: ChangeKind::Unchanged,
            });
        }
    }
    if !dry_run {
        atomic_write(path, expected, mode)?;
    }
    Ok(FileChange {
        path: path.to_owned(),
        action: if existed {
            ChangeKind::Updated
        } else {
            ChangeKind::Created
        },
    })
}

fn remove_owned_file(path: &Path, expected: &[u8], dry_run: bool) -> Result<FileChange, String> {
    let current = match fs::read(path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileChange {
                path: path.to_owned(),
                action: ChangeKind::Missing,
            });
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    if current != expected {
        return Ok(FileChange {
            path: path.to_owned(),
            action: ChangeKind::RetainedModified,
        });
    }
    if !dry_run {
        fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))?;
    }
    Ok(FileChange {
        path: path.to_owned(),
        action: ChangeKind::Removed,
    })
}

fn inspect_file(path: &Path, expected: &[u8]) -> Result<(), String> {
    let current = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if current != expected {
        return Err(format!("{} is stale or locally modified", path.display()));
    }
    Ok(())
}

fn pretty_json(value: &Value) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("serialize hooks.json: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("codex-wake"),
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        file.write_all(contents)
            .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", temporary.display()))?;
        fs::rename(&temporary, path).map_err(|error| format!("replace {}: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_if_empty(path: &Path) -> Result<(), String> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(format!(
            "remove empty directory {}: {error}",
            path.display()
        )),
    }
}
