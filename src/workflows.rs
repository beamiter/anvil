//! anvil's binding to the shared workflow library in `jterm_core::workflows`.
//!
//! Every jterm terminal grew its own copy of the "saved command with
//! parameters" subsystem, and the on-disk format is the whole point of it —
//! the four apps read the same library out of the same directories — so a
//! difference in what one app *accepted* was a difference in what a user's
//! file *meant* depending on which terminal opened it. Discovery, the bounded
//! reader, both parsers, validation and the template engine now live in
//! `jterm_core::workflows`; anvil's copy of them was the same code with a
//! different name and one guard missing. What is left here is the policy anvil
//! states for itself and the notebook asset lookup that lives in this file only
//! because it reuses the directory-search shape.
//!
//! # What anvil states, and why the shared code refuses to guess it
//!
//! - **The XDG backend is glib**, not the `dirs` crate the core defaults to.
//!   `gtk::glib::user_config_dir()` never fails and carries GTK's own fallback
//!   chain; `dirs::config_dir()` returns `None` with `HOME` unset. Those agree
//!   on a desktop and differ exactly where it would be invisible, so
//!   [`GlibDirs`] is passed in rather than inherited.
//! - **The app segment is [`crate::host::APP_NAME`]**, spelled out rather than
//!   read back from `jterm_core::identity`. `identity::init` runs in `main`,
//!   and no test binary calls it — `SearchPathSpec::for_current_app` would
//!   answer `None` here and anvil's own search-path assertions would then be
//!   guarding nothing. The override variable `ANVIL_WORKFLOW_DIR` is derived
//!   from that one segment, so anvil cannot look under one name while honouring
//!   another's variable.
//! - **The dev-tree tier is passed in.** `env!("CARGO_MANIFEST_DIR")` resolves
//!   against the crate being compiled, so evaluating it inside `jterm_core`
//!   would point every app at `jterm_core/scripts/workflows` while their
//!   bundled-library tests kept passing.
//! - **[`LoadOrder::Precedence`]**, anvil's existing order: the palette sorts
//!   by tier then fuzzy score with a stable sort, so ties — every entry when
//!   the query is empty — keep load order, and load order puts the user's own
//!   `~/.config/anvil/workflows` ahead of the installed and bundled examples.
//!
//! # What anvil gained, and what it therefore owes the user
//!
//! anvil was the copy whose bounded reader passed `O_NONBLOCK | O_CLOEXEC` and
//! not `O_NOFOLLOW`: a symlink planted in `~/.config/anvil/workflows/` pointing
//! at a world-writable file was followed, parsed, and its command became a
//! palette entry that gets typed at a prompt — while the same planted link was
//! refused by the other three terminals. The shared reader refuses it here too
//! now. But symlinking a file (or a whole directory) out of a dotfiles checkout
//! is something people do on purpose, and a workflow that silently stops
//! existing is indistinguishable from one that was never installed. So the
//! refusals are collected ([`refused_files`], [`scan`]) and `workflow_ops`
//! raises a toast when that set changes — the loader's log line is not a
//! user-visible surface. Only a symlinked *file* is refused; a symlinked
//! directory in the search path is still scanned.
//!
//! anvil's diagnostics report reads the same two functions. It used to carry a
//! second `toml|yaml|yml` predicate and an uncapped `read_dir` walk of every
//! workflow directory — a second implementation of this on-disk contract inside
//! one app, and the one place that ignored every bound the loader enforces.

use relm4::gtk;

use jterm_core::workflows::{
    workflow_files_in, DirSources, LoadOrder, SearchPathSpec, MAX_WORKFLOW_DIRECTORIES,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(crate) use jterm_core::workflows::{load_one, ArgsForm, Workflow};

/// Directory precedence, not alphabetical. See the module docs: the palette's
/// sort is stable and score-free for an empty query, so this *is* the order the
/// user sees when they open it.
const LOAD_ORDER: LoadOrder = LoadOrder::Precedence;

/// How many refused files one scan reports. The loader is bounded, so this
/// bounds the report built from it: a directory of 512 broken files must not
/// become 512 strings held on the UI thread.
const MAX_REFUSALS_REPORTED: usize = 64;

/// The XDG lookups anvil answers with glib rather than the `dirs` crate.
struct GlibDirs;

impl DirSources for GlibDirs {
    fn user_config_dir(&self) -> Option<PathBuf> {
        Some(gtk::glib::user_config_dir())
    }

    fn user_data_dir(&self) -> Option<PathBuf> {
        Some(gtk::glib::user_data_dir())
    }

    fn system_data_dirs(&self) -> Vec<PathBuf> {
        gtk::glib::system_data_dirs()
    }
}

/// anvil's half of the search path: directory segment, override variable, and
/// the source tree used during development.
fn search_path_spec() -> SearchPathSpec {
    SearchPathSpec::for_app(
        crate::host::APP_NAME,
        Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("scripts")
                .join("workflows"),
        ),
    )
}

/// Workflow search path in precedence order: user config,
/// `$ANVIL_WORKFLOW_DIR`, user data, each installed data directory, then the
/// source-tree examples.
pub(crate) fn workflow_dirs() -> Vec<PathBuf> {
    jterm_core::workflows::search_path(&search_path_spec(), &GlibDirs)
}

/// Load the library from `dirs` in anvil's order.
pub(crate) fn load_all(dirs: &[PathBuf]) -> Vec<Workflow> {
    jterm_core::workflows::load_all(dirs, LOAD_ORDER)
}

/// One completed pass over the search path: what loaded, and what did not.
#[derive(Clone, Debug, Default)]
pub(crate) struct LibraryScan {
    pub(crate) workflows: Vec<Workflow>,
    pub(crate) refused: Vec<(PathBuf, String)>,
}

/// Load the library and, in the same pass, find out which candidate files the
/// loader turned down.
pub(crate) fn scan(dirs: &[PathBuf]) -> LibraryScan {
    let workflows = load_all(dirs);
    let refused = refused_files(dirs, &workflows);
    LibraryScan { workflows, refused }
}

/// Every workflow-looking file under `dirs` that is not in `loaded`, paired
/// with the loader's reason for refusing it.
///
/// A file is re-read only when it is *not* among the loaded workflows, so the
/// common case — nothing broken — costs one directory listing per tier and no
/// extra file reads at all. That is what makes reporting refusals affordable on
/// the refresh path instead of only in the diagnostics report. Files that
/// loaded but lost a name collision to a higher-precedence directory are
/// re-read and return `Ok`, so shadowing (which is a feature) never shows up
/// here as breakage.
///
/// This deliberately reuses the loader's own `workflow_files_in` — the same
/// extension predicate and the same per-directory caps — rather than walking
/// the directories again. anvil already had a second, uncapped walk with its
/// own predicate, and that is precisely how a copy starts drifting from the
/// contract it is supposed to be reading.
pub(crate) fn refused_files(dirs: &[PathBuf], loaded: &[Workflow]) -> Vec<(PathBuf, String)> {
    let accepted: HashSet<&Path> = loaded
        .iter()
        .filter_map(|workflow| workflow.source_path.as_deref())
        .collect();
    let mut refused = Vec::new();
    for dir in dirs.iter().take(MAX_WORKFLOW_DIRECTORIES) {
        // Guarded the way the loader guards it: a missing tier is normal and
        // must not produce a "cannot list" warning per refresh.
        if !dir.is_dir() {
            continue;
        }
        for path in workflow_files_in(dir) {
            if accepted.contains(path.as_path()) {
                continue;
            }
            if let Err(reason) = load_one(&path) {
                refused.push((path, reason));
                if refused.len() >= MAX_REFUSALS_REPORTED {
                    return refused;
                }
            }
        }
    }
    refused
}

fn installed_asset_dirs(kind: &str) -> Vec<PathBuf> {
    asset_dirs_from(gtk::glib::system_data_dirs(), kind)
}

fn asset_dirs_from(data_dirs: impl IntoIterator<Item = PathBuf>, kind: &str) -> Vec<PathBuf> {
    data_dirs
        .into_iter()
        .map(|base| base.join(crate::host::APP_NAME).join(kind))
        .collect()
}

/// Locate the installed/development welcome notebook used by the command
/// center's first-run entry.
///
/// Deliberately not migrated to `jterm_core`: ember and frost each documented
/// not porting it because they have no notebook surface, and the shared module
/// has no business knowing a notebook asset layout. It lives here because it
/// reuses the directory-search *shape*, not the workflow contract.
pub(crate) fn welcome_notebook_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(asset_dir) = std::env::var_os("ANVIL_ASSET_DIR") {
        candidates.push(PathBuf::from(asset_dir).join("notebooks/welcome.jtnb.md"));
    }
    candidates.push(
        gtk::glib::user_data_dir()
            .join(crate::host::APP_NAME)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(label: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "anvil-workflows-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The engine's semantics are the core's tests. What anvil still owns is
    /// the four policy values it passes, and every one of them changes which
    /// files end up at a prompt if it silently changes.
    #[test]
    fn anvil_pins_its_segment_its_override_variable_and_precedence_order() {
        let spec = search_path_spec();
        assert_eq!(spec.app(), "anvil");
        assert_eq!(spec.env_var(), "ANVIL_WORKFLOW_DIR");
        assert_eq!(
            spec.dev_root(),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("scripts")
                    .join("workflows")
                    .as_path()
            ),
            "the source-tree tier must resolve against anvil's manifest, not the core's"
        );
        assert_eq!(LOAD_ORDER, LoadOrder::Precedence);

        // The tiers anvil's own spec contributes, asked in isolation from
        // whatever XDG this machine has: a `nix develop` shell alone puts more
        // than MAX_WORKFLOW_DIRECTORIES entries in XDG_DATA_DIRS, which is
        // enough to truncate the source-tree tier out of the real path — a
        // property of the environment, not of the policy under test.
        struct NoDirs;
        impl DirSources for NoDirs {
            fn user_config_dir(&self) -> Option<PathBuf> {
                None
            }
            fn user_data_dir(&self) -> Option<PathBuf> {
                None
            }
            fn system_data_dirs(&self) -> Vec<PathBuf> {
                Vec::new()
            }
        }
        assert_eq!(
            jterm_core::workflows::search_path(&spec, &NoDirs),
            [spec.dev_root().expect("anvil passes a source-tree tier")],
            "the bundled examples must remain a tier of anvil's search path"
        );

        // And glib really is the backend that is wired in.
        assert_eq!(
            workflow_dirs().first(),
            Some(&gtk::glib::user_config_dir().join("anvil").join("workflows")),
            "the user's own library must stay the highest-precedence tier"
        );
    }

    /// The contract test the other three terminals had and anvil did not:
    /// anvil could ship a bundled example its own validator rejects and nothing
    /// would fail. Every file must load. A workflow whose file declares every
    /// default must render immediately; an intentionally required field must
    /// fail with the same named missing-value error the dialog shows.
    #[test]
    fn every_bundled_workflow_loads_and_has_coherent_argument_defaults() {
        let dir = search_path_spec()
            .dev_root()
            .expect("anvil passes a source-tree tier")
            .to_path_buf();
        let candidates = workflow_files_in(&dir);
        assert!(
            !candidates.is_empty(),
            "scripts/workflows must ship examples"
        );

        let loaded = load_all(std::slice::from_ref(&dir));
        assert_eq!(
            loaded.len(),
            candidates.len(),
            "every bundled example must parse and validate; refused: {:?}",
            refused_files(std::slice::from_ref(&dir), &loaded)
        );
        for workflow in &loaded {
            let form = ArgsForm::new(workflow.clone());
            let missing = form.missing();
            match form.render() {
                Ok(_) => assert!(
                    missing.is_empty(),
                    "bundled '{}' declares unused required arguments: {missing:?}",
                    workflow.name
                ),
                Err(error) => {
                    assert!(
                        !missing.is_empty(),
                        "bundled '{}' failed despite having complete defaults: {error}",
                        workflow.name
                    );
                    assert!(
                        error.starts_with("missing values:"),
                        "{}: {error}",
                        workflow.name
                    );
                    for name in missing {
                        assert!(
                            error.split([',', ':']).any(|part| part.trim() == name),
                            "bundled '{}' did not name missing argument '{name}': {error}",
                            workflow.name
                        );
                    }
                }
            }
        }
    }

    /// anvil gained `O_NOFOLLOW` on migration, so a symlinked workflow file it
    /// used to load is now refused. That is the right call — it was the one
    /// terminal that loaded attacker-plantable content — but symlinking a file
    /// out of a dotfiles checkout is deliberate, so the refusal has to be
    /// reportable rather than a workflow that quietly stops existing.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_workflow_is_refused_and_the_refusal_is_reportable() {
        let dir = scratch_dir("symlink");
        let target = dir.join("real-target.yaml");
        std::fs::write(&target, "name: Linked\ncommand: echo linked\n").unwrap();
        std::fs::write(dir.join("plain.yaml"), "name: Plain\ncommand: echo plain\n").unwrap();
        let link = dir.join("linked.yaml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let dirs = vec![dir.clone()];
        let scan = scan(&dirs);
        // The link's target is itself a candidate in this directory, so
        // "Linked" is still loaded once — from the regular file, never through
        // the link.
        let sources: Vec<&Path> = scan
            .workflows
            .iter()
            .filter_map(|workflow| workflow.source_path.as_deref())
            .collect();
        assert!(sources.contains(&target.as_path()));
        assert!(!sources.contains(&link.as_path()));
        assert_eq!(
            scan.refused
                .iter()
                .map(|(path, _)| path.as_path())
                .collect::<Vec<_>>(),
            [link.as_path()],
            "the refused symlink must be named, not silently dropped"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A shadowed duplicate is the documented override feature, not breakage:
    /// it must never reach the user as a "skipped file" toast.
    #[test]
    fn a_name_shadowed_file_is_not_reported_as_refused() {
        let user = scratch_dir("shadow-user");
        let installed = scratch_dir("shadow-installed");
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
        std::fs::write(installed.join("broken.yaml"), "this: is not a workflow\n").unwrap();

        let dirs = vec![user.clone(), installed.clone()];
        let scan = scan(&dirs);
        assert_eq!(
            scan.workflows
                .iter()
                .find(|workflow| workflow.name == "Same")
                .map(|workflow| workflow.command.as_str()),
            Some("echo user"),
            "precedence order must keep the user's file"
        );
        assert_eq!(
            scan.refused
                .iter()
                .map(|(path, _)| path.as_path())
                .collect::<Vec<_>>(),
            [installed.join("broken.yaml").as_path()]
        );

        let _ = std::fs::remove_dir_all(user);
        let _ = std::fs::remove_dir_all(installed);
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
}
