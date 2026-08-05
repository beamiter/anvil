//! Parameterised command templates — Warp-style "workflows".
//!
//! A workflow is a TOML or YAML file: a name, a description, an optional shell,
//! an optional tag list, a command template with `{arg}` or `{{arg}}`
//! placeholders, and named arguments with optional defaults and descriptions.
//!
//! Files are loaded from `~/.config/anvil/workflows/`, installed XDG
//! data directories, and the development `scripts/workflows/` directory.
//! Parse failures are logged and skipped — one broken file never disables the
//! rest.
//!
//! The render step is intentionally tiny: named substitution plus literal
//! brace escapes, without a conditionals/loops templating language.
//!
//! Once loaded, workflows surface in the command palette as a third tier
//! (after actions and history) and via `:` prefix or `Action::OpenWorkflows`.

use relm4::gtk;

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_WORKFLOW_FILE_BYTES: u64 = 256 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_WORKFLOW_FILES_PER_DIRECTORY: usize = 512;
const MAX_WORKFLOWS: usize = 1_024;
const MAX_WORKFLOW_DIRECTORIES: usize = 64;
const MAX_WORKFLOW_NAME_BYTES: usize = 256;
const MAX_WORKFLOW_DESCRIPTION_BYTES: usize = 4 * 1024;
const MAX_WORKFLOW_COMMAND_BYTES: usize = 64 * 1024;
const MAX_WORKFLOW_TAGS: usize = 64;
const MAX_WORKFLOW_ARGS: usize = 64;
const MAX_WORKFLOW_FIELD_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Workflow {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional interpreter hint retained for shared workflow libraries.
    /// Workflows remain review-only and are never auto-executed.
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub args: Vec<WorkflowArg>,
    /// Source file the workflow was loaded from — useful for "edit workflow"
    /// shortcuts later; populated post-deserialize.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkflowArg {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Option<String>,
}

/// Load every `*.toml` / `*.yaml` / `*.yml` file under the given directories.
/// Missing directories are skipped; earlier directories win duplicate names.
pub(crate) fn load_all(dirs: &[PathBuf]) -> Vec<Workflow> {
    let mut out = Vec::new();
    let mut names = HashSet::new();
    'directories: for dir in dirs.iter().take(MAX_WORKFLOW_DIRECTORIES) {
        if !dir.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(err) => {
                log::warn!("workflows: cannot list {}: {err}", dir.display());
                continue;
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .take(MAX_DIRECTORY_ENTRIES)
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| {
                        e.eq_ignore_ascii_case("toml")
                            || e.eq_ignore_ascii_case("yaml")
                            || e.eq_ignore_ascii_case("yml")
                    })
                    .unwrap_or(false)
            })
            .take(MAX_WORKFLOW_FILES_PER_DIRECTORY)
            .collect();
        // Deterministic order so two runs with the same files produce the same
        // palette ordering — easier to keep muscle memory.
        paths.sort();
        for path in paths {
            match load_one(&path) {
                Ok(wf) => {
                    // Earlier directories have higher precedence, allowing a
                    // user workflow to replace an installed example by name.
                    if names.insert(wf.name.clone()) {
                        out.push(wf);
                        if out.len() >= MAX_WORKFLOWS {
                            break 'directories;
                        }
                    }
                }
                Err(err) => log::warn!("workflows: skipping {}: {err}", path.display()),
            }
        }
    }
    out
}

pub(crate) fn load_one(path: &Path) -> Result<Workflow, String> {
    let text = read_bounded_workflow(path)?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut wf: Workflow = match extension.as_str() {
        "toml" => toml::from_str(&text).map_err(|e| format!("parse TOML: {e}"))?,
        "yaml" | "yml" => serde_yaml::from_str(&text).map_err(|e| format!("parse YAML: {e}"))?,
        _ => return Err("unsupported workflow extension".to_string()),
    };
    validate_workflow(&wf)?;
    wf.source_path = Some(path.to_path_buf());
    Ok(wf)
}

fn read_bounded_workflow(path: &Path) -> Result<String, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("read: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect: {error}"))?;
    if !metadata.is_file() {
        return Err("source is not a regular file".to_string());
    }
    if metadata.len() > MAX_WORKFLOW_FILE_BYTES {
        return Err(format!(
            "source exceeds the {MAX_WORKFLOW_FILE_BYTES}-byte limit"
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_WORKFLOW_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read: {error}"))?;
    if bytes.len() as u64 > MAX_WORKFLOW_FILE_BYTES {
        return Err(format!(
            "source exceeds the {MAX_WORKFLOW_FILE_BYTES}-byte limit"
        ));
    }
    String::from_utf8(bytes).map_err(|error| format!("source is not UTF-8: {error}"))
}

fn validate_workflow(workflow: &Workflow) -> Result<(), String> {
    validate_display_field("name", &workflow.name, MAX_WORKFLOW_NAME_BYTES, false)?;
    validate_display_field(
        "description",
        &workflow.description,
        MAX_WORKFLOW_DESCRIPTION_BYTES,
        true,
    )?;
    if workflow.command.trim().is_empty() {
        return Err("workflow has empty command".to_string());
    }
    if workflow.command.len() > MAX_WORKFLOW_COMMAND_BYTES {
        return Err(format!(
            "workflow command exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
        ));
    }
    crate::review_input::validate(&workflow.command)
        .map_err(|error| format!("command is unsafe for review-only insertion: {error}"))?;
    if crate::text_safety::contains_visual_spoof(&workflow.command) {
        return Err("workflow command contains an invisible or bidirectional character".into());
    }
    if workflow.tags.len() > MAX_WORKFLOW_TAGS {
        return Err(format!("workflow has more than {MAX_WORKFLOW_TAGS} tags"));
    }
    for tag in &workflow.tags {
        validate_display_field("tag", tag, MAX_WORKFLOW_FIELD_BYTES, false)?;
    }
    if let Some(shell) = &workflow.shell {
        validate_display_field("shell", shell, MAX_WORKFLOW_FIELD_BYTES, false)?;
    }
    if workflow.args.len() > MAX_WORKFLOW_ARGS {
        return Err(format!(
            "workflow has more than {MAX_WORKFLOW_ARGS} arguments"
        ));
    }
    let mut names = HashSet::new();
    for argument in &workflow.args {
        validate_display_field(
            "argument name",
            &argument.name,
            MAX_WORKFLOW_FIELD_BYTES,
            false,
        )?;
        if !names.insert(argument.name.as_str()) {
            return Err(format!("duplicate workflow argument '{}'", argument.name));
        }
        validate_display_field(
            "argument description",
            &argument.description,
            MAX_WORKFLOW_DESCRIPTION_BYTES,
            true,
        )?;
        if let Some(default) = &argument.default {
            if default.len() > MAX_WORKFLOW_COMMAND_BYTES {
                return Err(format!(
                    "default for '{}' exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes",
                    argument.name
                ));
            }
            if default
                .chars()
                .any(|ch| ch.is_control() || crate::text_safety::is_visual_spoof(ch))
            {
                return Err(format!(
                    "default for '{}' is unsafe for command insertion",
                    argument.name
                ));
            }
        }
    }
    Ok(())
}

fn validate_display_field(
    label: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), String> {
    if !allow_empty && value.trim().is_empty() {
        return Err(format!("workflow has empty {label}"));
    }
    if value.len() > max_bytes {
        return Err(format!("workflow {label} exceeds {max_bytes} bytes"));
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || crate::text_safety::is_visual_spoof(ch))
    {
        return Err(format!(
            "workflow {label} contains a control, invisible, or bidirectional character"
        ));
    }
    Ok(())
}

/// Standard config dir: `<XDG_CONFIG_HOME>/anvil/workflows/`.
pub(crate) fn user_workflow_dir() -> PathBuf {
    let base: PathBuf = gtk::glib::user_config_dir();
    base.join("anvil").join("workflows")
}

fn installed_asset_dirs(kind: &str) -> Vec<PathBuf> {
    asset_dirs_from(gtk::glib::system_data_dirs(), kind)
}

fn asset_dirs_from(data_dirs: impl IntoIterator<Item = PathBuf>, kind: &str) -> Vec<PathBuf> {
    data_dirs
        .into_iter()
        .map(|base| base.join("anvil").join(kind))
        .collect()
}

/// Workflow search path in precedence order. User-authored config wins,
/// followed by installed examples, then the source-tree examples used during
/// development. `ANVIL_WORKFLOW_DIR` may add one or more platform-separated
/// directories without replacing the standard locations.
pub(crate) fn workflow_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![user_workflow_dir()];
    if let Some(extra) = std::env::var_os("ANVIL_WORKFLOW_DIR") {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs.push(gtk::glib::user_data_dir().join("anvil").join("workflows"));
    dirs.extend(installed_asset_dirs("workflows"));
    dirs.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("workflows"),
    );
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for dir in dirs.into_iter().take(MAX_WORKFLOW_DIRECTORIES) {
        if seen.insert(dir.clone()) {
            unique.push(dir);
        }
    }
    unique
}

/// Locate the installed/development welcome notebook used by the command
/// center's first-run entry.
pub(crate) fn welcome_notebook_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(asset_dir) = std::env::var_os("ANVIL_ASSET_DIR") {
        candidates.push(PathBuf::from(asset_dir).join("notebooks/welcome.jtnb.md"));
    }
    candidates.push(
        gtk::glib::user_data_dir()
            .join("anvil")
            .join("notebooks")
            .join("welcome.jtnb.md"),
    );
    candidates.extend(
        installed_asset_dirs("notebooks")
            .into_iter()
            .map(|dir| dir.join("welcome.jtnb.md")),
    );
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("notebooks")
            .join("welcome.jtnb.md"),
    );
    candidates.into_iter().find(|path| path.is_file())
}

/// Substitute both native `{name}` and shared-library `{{name}}` placeholders.
/// Unknown single-brace placeholders stay visible. Double braces without a
/// matching binding emit one literal brace pair, mirroring `format!` escapes.
/// Iteration advances by Unicode scalar value, never by raw UTF-8 byte.
pub(crate) fn substitute(template: &str, bindings: &[(String, String)]) -> Result<String, String> {
    render_template(template, bindings, &HashSet::new()).map(|(rendered, _)| rendered)
}

fn render_template(
    template: &str,
    bindings: &[(String, String)],
    missing_bindings: &HashSet<String>,
) -> Result<(String, Vec<String>), String> {
    if template.len() > MAX_WORKFLOW_COMMAND_BYTES {
        return Err(format!(
            "workflow command exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
        ));
    }
    let mut out = String::with_capacity(template.len().min(MAX_WORKFLOW_COMMAND_BYTES));
    let bytes = template.as_bytes();
    let mut missing = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                if let Some(end) = find_close(bytes, i + 2) {
                    let name = template[i + 2..end].trim();
                    if let Some((_, value)) = bindings.iter().find(|(key, _)| key == name) {
                        push_rendered(&mut out, value)?;
                        i = end + 2;
                        continue;
                    }
                    if missing_bindings.contains(name) {
                        if !missing.iter().any(|entry| entry == name) {
                            missing.push(name.to_owned());
                        }
                        i = end + 2;
                        continue;
                    }
                    // No binding means `{{...}}` is a literal-brace escape.
                    push_rendered(&mut out, "{")?;
                    i += 2;
                    continue;
                }
                // Preserve an unterminated pair exactly as authored.
                push_rendered(&mut out, "{")?;
                i += 1;
                continue;
            }

            if let Some(end_relative) = bytes[i + 1..].iter().position(|byte| *byte == b'}') {
                let end = i + 1 + end_relative;
                let name = template[i + 1..end].trim();
                if let Some((_, value)) = bindings.iter().find(|(key, _)| key == name) {
                    push_rendered(&mut out, value)?;
                } else if missing_bindings.contains(name) {
                    if !missing.iter().any(|entry| entry == name) {
                        missing.push(name.to_owned());
                    }
                } else {
                    push_rendered(&mut out, &template[i..=end])?;
                }
                i = end + 1;
                continue;
            }
        } else if bytes[i] == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            push_rendered(&mut out, "}")?;
            i += 2;
            continue;
        }

        let character = template[i..]
            .chars()
            .next()
            .expect("i always points to a UTF-8 boundary");
        let mut encoded = [0_u8; 4];
        push_rendered(&mut out, character.encode_utf8(&mut encoded))?;
        i += character.len_utf8();
    }

    Ok((out, missing))
}

fn push_rendered(output: &mut String, addition: &str) -> Result<(), String> {
    if output.len().saturating_add(addition.len()) > MAX_WORKFLOW_COMMAND_BYTES {
        return Err(format!(
            "rendered command exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
        ));
    }
    output.push_str(addition);
    Ok(())
}

/// Render a workflow using caller values and declared defaults. Missing
/// declared placeholders are reported, and the final command crosses the same
/// review-input safety boundary as history/AI/file insertions.
pub(crate) fn render(
    workflow: &Workflow,
    values: &HashMap<String, String>,
) -> Result<String, String> {
    validate_workflow(workflow)?;
    if values.len() > MAX_WORKFLOW_ARGS {
        return Err(format!(
            "workflow received more than {MAX_WORKFLOW_ARGS} values"
        ));
    }
    for (name, value) in values {
        validate_display_field("value name", name, MAX_WORKFLOW_FIELD_BYTES, false)?;
        if value.len() > MAX_WORKFLOW_COMMAND_BYTES {
            return Err(format!(
                "value for '{name}' exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
            ));
        }
        if value
            .chars()
            .any(|ch| ch.is_control() || crate::text_safety::is_visual_spoof(ch))
        {
            return Err(format!(
                "value for '{name}' is unsafe for review-only insertion"
            ));
        }
    }
    let mut bindings: Vec<(String, String)> = values
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut missing_bindings = HashSet::new();
    for argument in &workflow.args {
        if values.contains_key(&argument.name) {
            continue;
        }
        if let Some(default) = &argument.default {
            bindings.push((argument.name.clone(), default.clone()));
        } else {
            missing_bindings.insert(argument.name.clone());
        }
    }

    let (out, missing) = render_template(&workflow.command, &bindings, &missing_bindings)?;
    if !missing.is_empty() {
        return Err(format!("missing values: {}", missing.join(", ")));
    }
    crate::review_input::validate(&out)
        .map_err(|error| format!("command is unsafe for review-only insertion: {error}"))?;
    Ok(out)
}

fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(name: &str, command: &str, args: &[(&str, Option<&str>)]) -> Workflow {
        Workflow {
            name: name.to_string(),
            description: String::new(),
            command: command.to_string(),
            tags: Vec::new(),
            shell: None,
            args: args
                .iter()
                .map(|(n, d)| WorkflowArg {
                    name: n.to_string(),
                    description: String::new(),
                    default: d.map(|s| s.to_string()),
                })
                .collect(),
            source_path: None,
        }
    }

    #[test]
    fn render_substitutes_single_placeholder() {
        let w = wf("t", "git rebase -i {{target}}", &[("target", None)]);
        let mut v = HashMap::new();
        v.insert("target".to_string(), "origin/main".to_string());
        assert_eq!(render(&w, &v).unwrap(), "git rebase -i origin/main");
    }

    #[test]
    fn render_uses_declared_default_when_value_missing() {
        let w = wf(
            "t",
            "echo {{greeting}} {{name}}",
            &[("greeting", Some("hi")), ("name", Some("world"))],
        );
        let v = HashMap::new();
        assert_eq!(render(&w, &v).unwrap(), "echo hi world");
    }

    #[test]
    fn render_reports_missing_placeholder() {
        let w = wf("t", "kill -9 {{pid}}", &[("pid", None)]);
        let v = HashMap::new();
        let err = render(&w, &v).unwrap_err();
        assert!(err.contains("pid"), "got {err}");
    }

    #[test]
    fn render_leaves_unterminated_braces_alone() {
        let w = wf("t", "echo {{not_closed", &[]);
        let v = HashMap::new();
        // Without a closing `}}` we treat the rest as literal text rather than
        // erroring — keeps the failure mode predictable.
        assert_eq!(render(&w, &v).unwrap(), "echo {{not_closed");
    }

    #[test]
    fn render_handles_multiple_occurrences_of_same_arg() {
        let w = wf("t", "cp {{f}} {{f}}.bak", &[("f", None)]);
        let mut v = HashMap::new();
        v.insert("f".to_string(), "config.toml".to_string());
        assert_eq!(render(&w, &v).unwrap(), "cp config.toml config.toml.bak");
    }

    #[test]
    fn render_supports_unicode_both_placeholder_styles_and_literal_braces() {
        let w = wf(
            "发布",
            "发布 {服务} 到 {{环境}}，保留 {{a,b}} 🚀",
            &[("服务", None), ("环境", None)],
        );
        let values = HashMap::from([
            ("服务".to_string(), "接口".to_string()),
            ("环境".to_string(), "生产".to_string()),
        ]);
        assert_eq!(
            render(&w, &values).unwrap(),
            "发布 接口 到 生产，保留 {a,b} 🚀"
        );
        assert_eq!(
            substitute(
                "你好 {name} / {{name}} / {{x,y}}",
                &[("name".into(), "世界".into())]
            )
            .unwrap(),
            "你好 世界 / 世界 / {x,y}"
        );
    }

    #[test]
    fn render_rejects_control_characters_introduced_by_values() {
        let w = wf("unsafe", "echo {value}", &[("value", None)]);
        let values = HashMap::from([("value".to_string(), "ok\nrm -rf /".to_string())]);
        assert!(render(&w, &values)
            .unwrap_err()
            .contains("unsafe for review-only insertion"));
    }

    #[test]
    fn load_all_skips_invalid_files_but_returns_good_ones() {
        let dir = tempdir();
        std::fs::write(dir.join("a.yaml"), "name: A\ncommand: echo a\n").unwrap();
        std::fs::write(dir.join("b.yaml"), "this: is not a workflow\n").unwrap();
        std::fs::write(dir.join("c.yaml"), "name: C\ncommand: echo c\n").unwrap();
        let loaded = load_all(std::slice::from_ref(&dir));
        let names: Vec<&str> = loaded.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["A", "C"], "names actually {:?}", names);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_one_rejects_empty_command() {
        let dir = tempdir();
        let p = dir.join("bad.yaml");
        std::fs::write(&p, "name: X\ncommand: \"\"\n").unwrap();
        let err = load_one(&p).unwrap_err();
        assert!(err.contains("empty command"), "got {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loads_toml_and_preserves_metadata() {
        let dir = tempdir();
        let path = dir.join("deploy.toml");
        std::fs::write(
            &path,
            r#"name = "部署"
description = "发布服务"
command = "deploy {service}"
tags = ["ops", "中文"]
shell = "fish"

[[args]]
name = "service"
description = "服务名"
default = "api"
"#,
        )
        .unwrap();
        let workflow = load_one(&path).unwrap();
        assert_eq!(workflow.name, "部署");
        assert_eq!(workflow.tags, ["ops", "中文"]);
        assert_eq!(workflow.shell.as_deref(), Some("fish"));
        assert_eq!(workflow.args[0].default.as_deref(), Some("api"));
        assert_eq!(workflow.source_path.as_deref(), Some(path.as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn earlier_directory_wins_when_names_collide_across_formats() {
        let user = tempdir();
        let installed = tempdir();
        std::fs::write(
            user.join("override.toml"),
            "name = 'Same'\ncommand = 'echo user'\n",
        )
        .unwrap();
        std::fs::write(
            installed.join("same.yaml"),
            "name: Same\ncommand: echo installed\n",
        )
        .unwrap();
        std::fs::write(
            installed.join("other.yml"),
            "name: Other\ncommand: echo other\n",
        )
        .unwrap();

        let loaded = load_all(&[user.clone(), installed.clone()]);
        assert_eq!(loaded.iter().filter(|wf| wf.name == "Same").count(), 1);
        assert_eq!(
            loaded.iter().find(|wf| wf.name == "Same").unwrap().command,
            "echo user"
        );
        assert!(loaded.iter().any(|wf| wf.name == "Other"));
        let _ = std::fs::remove_dir_all(user);
        let _ = std::fs::remove_dir_all(installed);
    }

    #[test]
    fn load_one_rejects_control_character_commands() {
        let dir = tempdir();
        let path = dir.join("unsafe.yaml");
        std::fs::write(&path, "name: Unsafe\ncommand: \"echo\\tsecret\"\n").unwrap();
        assert!(load_one(&path)
            .unwrap_err()
            .contains("unsafe for review-only insertion"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workflow_files_and_rendered_commands_are_strictly_bounded() {
        let dir = tempdir();
        let path = dir.join("oversized.yaml");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_WORKFLOW_FILE_BYTES + 1).unwrap();
        assert!(load_one(&path).unwrap_err().contains("byte limit"));

        let repeated = "{{value}}".repeat(4_000);
        let workflow = wf("bounded", &repeated, &[("value", None)]);
        let values = HashMap::from([("value".to_string(), "x".repeat(MAX_WORKFLOW_COMMAND_BYTES))]);
        assert!(render(&workflow, &values)
            .unwrap_err()
            .contains("rendered command exceeds"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn workflow_fifo_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempdir();
        let path = dir.join("blocked.yaml");
        let path_c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path_c is a live NUL-terminated pathname for this call.
        assert_eq!(unsafe { nix::libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(load_one(&path).unwrap_err().contains("not a regular file"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workflow_metadata_rejects_duplicates_and_visual_spoofing() {
        let mut duplicate = wf("duplicate", "echo ok", &[("x", None), ("x", Some("ok"))]);
        assert!(validate_workflow(&duplicate)
            .unwrap_err()
            .contains("duplicate workflow argument"));

        duplicate.args.truncate(1);
        duplicate.name = "safe\u{202e}txt".into();
        assert!(validate_workflow(&duplicate)
            .unwrap_err()
            .contains("bidirectional"));
        duplicate.name = "safe".into();
        duplicate.command = "echo safe\u{200b}hidden".into();
        assert!(validate_workflow(&duplicate)
            .unwrap_err()
            .contains("bidirectional"));
        duplicate.command = "echo safe\u{e0020}hidden".into();
        assert!(validate_workflow(&duplicate)
            .unwrap_err()
            .contains("bidirectional"));
    }

    #[test]
    fn installed_assets_follow_every_system_data_directory() {
        assert_eq!(
            asset_dirs_from(
                [PathBuf::from("/usr/share"), PathBuf::from("/app/share")],
                "workflows"
            ),
            [
                PathBuf::from("/usr/share/anvil/workflows"),
                PathBuf::from("/app/share/anvil/workflows")
            ]
        );
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "anvil-workflows-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
