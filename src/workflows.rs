//! Parameterised command templates — Warp-style "workflows".
//!
//! A workflow is a TOML or YAML file: a name, a description, an optional shell,
//! an optional tag list, a command template with `{arg}` or `{{arg}}`
//! placeholders, and named arguments with optional defaults and descriptions.
//!
//! Files are loaded from `~/.config/jterm1/workflows/`, installed XDG
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
use std::path::{Path, PathBuf};

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
    for dir in dirs {
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
                    }
                }
                Err(err) => log::warn!("workflows: skipping {}: {err}", path.display()),
            }
        }
    }
    out
}

pub(crate) fn load_one(path: &Path) -> Result<Workflow, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
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
    if wf.name.trim().is_empty() {
        return Err("workflow has empty name".to_string());
    }
    if wf.command.trim().is_empty() {
        return Err("workflow has empty command".to_string());
    }
    crate::review_input::validate(&wf.command)
        .map_err(|error| format!("command is unsafe for review-only insertion: {error}"))?;
    wf.source_path = Some(path.to_path_buf());
    Ok(wf)
}

/// Standard config dir: `<XDG_CONFIG_HOME>/jterm1/workflows/`.
pub(crate) fn user_workflow_dir() -> PathBuf {
    let base: PathBuf = gtk::glib::user_config_dir();
    base.join("jterm1").join("workflows")
}

fn installed_asset_dirs(kind: &str) -> Vec<PathBuf> {
    asset_dirs_from(gtk::glib::system_data_dirs(), kind)
}

fn asset_dirs_from(data_dirs: impl IntoIterator<Item = PathBuf>, kind: &str) -> Vec<PathBuf> {
    data_dirs
        .into_iter()
        .map(|base| base.join("jterm1").join(kind))
        .collect()
}

/// Workflow search path in precedence order. User-authored config wins,
/// followed by installed examples, then the source-tree examples used during
/// development. `JTERM1_WORKFLOW_DIR` may add one or more platform-separated
/// directories without replacing the standard locations.
pub(crate) fn workflow_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![user_workflow_dir()];
    if let Some(extra) = std::env::var_os("JTERM1_WORKFLOW_DIR") {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs.push(gtk::glib::user_data_dir().join("jterm1").join("workflows"));
    dirs.extend(installed_asset_dirs("workflows"));
    dirs.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("workflows"),
    );
    let mut unique = Vec::new();
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

/// Locate the installed/development welcome notebook used by the command
/// center's first-run entry.
pub(crate) fn welcome_notebook_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(asset_dir) = std::env::var_os("JTERM1_ASSET_DIR") {
        candidates.push(PathBuf::from(asset_dir).join("notebooks/welcome.jtnb.md"));
    }
    candidates.push(
        gtk::glib::user_data_dir()
            .join("jterm1")
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
pub(crate) fn substitute(template: &str, bindings: &[(String, String)]) -> String {
    render_template(template, bindings, &HashSet::new()).0
}

fn render_template(
    template: &str,
    bindings: &[(String, String)],
    missing_bindings: &HashSet<String>,
) -> (String, Vec<String>) {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut missing = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                if let Some(end) = find_close(bytes, i + 2) {
                    let name = template[i + 2..end].trim();
                    if let Some((_, value)) = bindings.iter().find(|(key, _)| key == name) {
                        out.push_str(value);
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
                    out.push('{');
                    i += 2;
                    continue;
                }
                // Preserve an unterminated pair exactly as authored.
                out.push('{');
                i += 1;
                continue;
            }

            if let Some(end_relative) = bytes[i + 1..].iter().position(|byte| *byte == b'}') {
                let end = i + 1 + end_relative;
                let name = template[i + 1..end].trim();
                if let Some((_, value)) = bindings.iter().find(|(key, _)| key == name) {
                    out.push_str(value);
                } else if missing_bindings.contains(name) {
                    if !missing.iter().any(|entry| entry == name) {
                        missing.push(name.to_owned());
                    }
                } else {
                    out.push_str(&template[i..=end]);
                }
                i = end + 1;
                continue;
            }
        } else if bytes[i] == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            out.push('}');
            i += 2;
            continue;
        }

        let character = template[i..]
            .chars()
            .next()
            .expect("i always points to a UTF-8 boundary");
        out.push(character);
        i += character.len_utf8();
    }

    (out, missing)
}

/// Render a workflow using caller values and declared defaults. Missing
/// declared placeholders are reported, and the final command crosses the same
/// review-input safety boundary as history/AI/file insertions.
pub(crate) fn render(
    workflow: &Workflow,
    values: &HashMap<String, String>,
) -> Result<String, String> {
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

    let (out, missing) = render_template(&workflow.command, &bindings, &missing_bindings);
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
            ),
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
    fn installed_assets_follow_every_system_data_directory() {
        assert_eq!(
            asset_dirs_from(
                [PathBuf::from("/usr/share"), PathBuf::from("/app/share")],
                "workflows"
            ),
            [
                PathBuf::from("/usr/share/jterm1/workflows"),
                PathBuf::from("/app/share/jterm1/workflows")
            ]
        );
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "jterm1-workflows-test-{}-{}",
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
