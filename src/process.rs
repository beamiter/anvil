//! App-facing names for the shared process helpers.
//!
//! Shell quoting, restorable-command classification, the `/proc` probes, and
//! child-process lifecycle management all live in `jterm_core::process`
//! (seeded from this file); they are re-exported here so call sites keep their
//! `crate::process::` paths.

pub(crate) use jterm_core::process::{
    command_requires_block_integration, command_uses_external_cwd, foreground_process_name,
    foreground_uses_external_cwd, observed_ssh_command, restorable_command, shell_quote_argv_for,
    shell_quote_path, ChildLifecycle, EscalationPolicy, ObservedSshCommand, ReapOwner,
};
