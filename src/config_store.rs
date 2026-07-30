//! Transactional configuration persistence, validation, and backup recovery.
//!
//! jterm1 is a `NON_UNIQUE` application: several windows may hold an in-memory
//! configuration at once. This module prevents an older window from silently
//! overwriting a newer on-disk edit by combining a process-safe lock with an
//! optimistic content revision. Writes use unique sibling temporary files,
//! durable renames, and two rotating known-good backups.

use relm4::gtk;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gtk::gdk::RGBA;

use crate::cli::ReportFormat;
use crate::config::{self, Config, TerminalMode};
use crate::keybindings::Action;
use jterm_core::keybindings::{is_unbind_token, parse, Chord};

const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ConfigRevision {
    Missing,
    Present { content: Box<[u8]>, hash: u64 },
}

impl fmt::Debug for ConfigRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("Missing"),
            Self::Present { content, hash } => f
                .debug_struct("Present")
                .field("bytes", &content.len())
                .field("hash", hash)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ConfigWriteError {
    Conflict { path: PathBuf },
    Locked { path: PathBuf },
    RevisionUnavailable { path: PathBuf },
    InvalidConfig { path: PathBuf, errors: usize },
    BackupUnavailable { path: PathBuf },
    Io(String),
}

impl ConfigWriteError {
    pub(crate) fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

impl std::error::Error for ConfigWriteError {}

impl fmt::Display for ConfigWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { path } => write!(
                f,
                "{} changed in another window or editor; reload it before saving",
                path.display()
            ),
            Self::Locked { path } => write!(
                f,
                "timed out waiting for the configuration write lock {}",
                path.display()
            ),
            Self::RevisionUnavailable { path } => write!(
                f,
                "cannot safely save {} because its starting revision is unavailable",
                path.display()
            ),
            Self::InvalidConfig { path, errors } => write!(
                f,
                "refusing to overwrite {} because validation found {errors} error(s)",
                path.display()
            ),
            Self::BackupUnavailable { path } => write!(
                f,
                "no valid configuration backup is available for {}",
                path.display()
            ),
            Self::Io(message) => f.write_str(message),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigLockStatus {
    Clear,
    Active,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConfigIssueSeverity {
    Warning,
    Error,
}

impl ConfigIssueSeverity {
    fn label(self) -> &'static str {
        match self {
            Self::Warning => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConfigIssue {
    severity: ConfigIssueSeverity,
    key: String,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConfigValidationReport {
    path: String,
    exists: bool,
    issues: Vec<ConfigIssue>,
    errors: usize,
    warnings: usize,
}

impl ConfigValidationReport {
    fn new(path: &Path, exists: bool) -> Self {
        Self {
            path: path.display().to_string(),
            exists,
            issues: Vec::new(),
            errors: 0,
            warnings: 0,
        }
    }

    fn push(
        &mut self,
        severity: ConfigIssueSeverity,
        key: impl Into<String>,
        message: impl Into<String>,
    ) {
        match severity {
            ConfigIssueSeverity::Warning => self.warnings += 1,
            ConfigIssueSeverity::Error => self.errors += 1,
        }
        self.issues.push(ConfigIssue {
            severity,
            key: key.into(),
            message: message.into(),
        });
    }

    fn warning(&mut self, key: impl Into<String>, message: impl Into<String>) {
        self.push(ConfigIssueSeverity::Warning, key, message);
    }

    fn error(&mut self, key: impl Into<String>, message: impl Into<String>) {
        self.push(ConfigIssueSeverity::Error, key, message);
    }

    pub(crate) fn exists(&self) -> bool {
        self.exists
    }

    pub(crate) fn errors(&self) -> usize {
        self.errors
    }

    pub(crate) fn warnings(&self) -> usize {
        self.warnings
    }

    pub(crate) fn healthy(&self) -> bool {
        self.errors == 0
    }
}

fn io_error(operation: &str, path: &Path, error: impl fmt::Display) -> ConfigWriteError {
    ConfigWriteError::Io(format!("{operation} {}: {error}", path.display()))
}

fn fingerprint(bytes: &[u8]) -> ConfigRevision {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    ConfigRevision::Present {
        content: bytes.to_vec().into_boxed_slice(),
        hash,
    }
}

fn revision_from_content(content: Option<&[u8]>) -> ConfigRevision {
    content.map_or(ConfigRevision::Missing, fingerprint)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, ConfigWriteError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("read", path, error)),
    }
}

fn revision_at(path: &Path) -> Result<ConfigRevision, ConfigWriteError> {
    let content = read_optional(path)?;
    Ok(revision_from_content(content.as_deref()))
}

pub(crate) fn current_revision() -> Result<ConfigRevision, ConfigWriteError> {
    revision_at(&config::config_file_path())
}

fn backup_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.bak")
}

fn secondary_backup_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.bak.1")
}

fn before_restore_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.before-restore")
}

fn lock_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.lock")
}

pub(crate) fn backup_paths() -> [PathBuf; 2] {
    let path = config::config_file_path();
    [backup_path_for(&path), secondary_backup_path_for(&path)]
}

#[cfg(unix)]
fn try_lock_exclusive(file: &fs::File) -> io::Result<bool> {
    // SAFETY: `file` owns a live descriptor for the duration of this call;
    // `flock` neither retains pointers nor accesses Rust-managed memory.
    let result =
        unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == nix::libc::EAGAIN || code == nix::libc::EWOULDBLOCK)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &fs::File) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "configuration locking is only supported on Unix",
    ))
}

#[cfg(unix)]
fn unlock(file: &fs::File) {
    // SAFETY: see `try_lock_exclusive`; the descriptor remains live here.
    if unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_UN) } != 0 {
        log::warn!(
            "Failed to release configuration write lock: {}",
            io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn unlock(_file: &fs::File) {}

fn open_lock_file(path: &Path) -> Result<fs::File, ConfigWriteError> {
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| io_error("open lock", path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("set permissions on", path, error))?;
    }
    Ok(file)
}

pub(crate) fn lock_status() -> ConfigLockStatus {
    let path = lock_path_for(&config::config_file_path());
    if !path.exists() {
        return ConfigLockStatus::Clear;
    }
    let Ok(file) = open_lock_file(&path) else {
        return ConfigLockStatus::Unavailable;
    };
    match try_lock_exclusive(&file) {
        Ok(true) => {
            unlock(&file);
            ConfigLockStatus::Clear
        }
        Ok(false) => ConfigLockStatus::Active,
        Err(_) => ConfigLockStatus::Unavailable,
    }
}

struct ConfigFileLock {
    file: fs::File,
}

impl ConfigFileLock {
    fn acquire(config_path: &Path) -> Result<Self, ConfigWriteError> {
        let path = lock_path_for(config_path);
        let file = open_lock_file(&path)?;
        let start = Instant::now();
        loop {
            match try_lock_exclusive(&file) {
                Ok(true) => return Ok(Self { file }),
                Ok(false) if start.elapsed() < LOCK_TIMEOUT => {
                    thread::sleep(Duration::from_millis(25));
                }
                Ok(false) => return Err(ConfigWriteError::Locked { path }),
                Err(error) => return Err(io_error("lock", &path, error)),
            }
        }
    }
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

fn unique_sibling(target: &Path, label: &str) -> Result<PathBuf, ConfigWriteError> {
    let parent = target.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", target.display()))
    })?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ConfigWriteError::Io(format!("{} has no file name", target.display())))?;
    let nonce = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".{name}.{label}.{}.{}", std::process::id(), nonce)))
}

fn stage_private_file(
    target: &Path,
    label: &str,
    contents: &[u8],
) -> Result<PathBuf, ConfigWriteError> {
    for _ in 0..16 {
        let path = unique_sibling(target, label)?;
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("create temporary file", &path, error)),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(io_error("set permissions on", &path, error));
            }
        }
        if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(io_error("write", &path, error));
        }
        return Ok(path);
    }
    Err(ConfigWriteError::Io(format!(
        "could not allocate a unique temporary file beside {}",
        target.display()
    )))
}

fn sync_parent(path: &Path) -> Result<(), ConfigWriteError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", path.display()))
    })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_error("sync directory", parent, error))
}

fn replace_with_staged(staged: &Path, target: &Path) -> Result<(), ConfigWriteError> {
    fs::rename(staged, target).map_err(|error| io_error("replace", target, error))?;
    sync_parent(target)
}

fn atomic_replace(target: &Path, contents: &[u8]) -> Result<(), ConfigWriteError> {
    let staged = stage_private_file(target, "tmp", contents)?;
    if let Err(error) = replace_with_staged(&staged, target) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok(())
}

fn rotate_backups(config_path: &Path, current: &[u8]) -> Result<(), ConfigWriteError> {
    let primary = backup_path_for(config_path);
    let secondary = secondary_backup_path_for(config_path);
    if let Some(previous_primary) = read_optional(&primary)? {
        atomic_replace(&secondary, &previous_primary)?;
    }
    atomic_replace(&primary, current)
}

fn apply_config_to_table(config: &Config, table: &mut toml::Table) {
    table.insert("opacity".into(), toml::Value::Float(config.window_opacity));
    table.insert(
        "scrollback".into(),
        toml::Value::Integer(config.terminal_scrollback_lines as i64),
    );
    table.insert("font".into(), toml::Value::String(config.font_desc.clone()));
    table.insert(
        "font_scale".into(),
        toml::Value::Float(config.default_font_scale),
    );
    table.insert(
        "theme".into(),
        toml::Value::String(config.theme_name.clone()),
    );
    table.insert(
        "terminal_mode".into(),
        toml::Value::String(
            match config.terminal_mode {
                TerminalMode::Block => "block",
                TerminalMode::Vte => "vte",
            }
            .to_string(),
        ),
    );
    table.insert(
        "tab_placement".into(),
        toml::Value::String(config.tab_placement.as_str().to_string()),
    );
    table.insert(
        "sidebar_view".into(),
        toml::Value::String(config.sidebar_view.as_str().to_string()),
    );
    table.insert(
        "sidebar_visible".into(),
        toml::Value::Boolean(config.sidebar_visible),
    );
    table.insert(
        "jsh_update_check".into(),
        toml::Value::String(config.jsh_update_check.as_str().to_string()),
    );
    table.insert(
        "sidebar_width".into(),
        toml::Value::Integer(config.sidebar_width as i64),
    );
    table.insert(
        "tab_width".into(),
        toml::Value::Integer(config.tab_width as i64),
    );
    table.insert(
        "block_compact".into(),
        toml::Value::Boolean(config.block_compact),
    );
    table.insert(
        "command_history_enabled".into(),
        toml::Value::Boolean(config.command_history_enabled),
    );
    table.insert("ai_enabled".into(), toml::Value::Boolean(config.ai_enabled));
    table.insert(
        "ai_provider".into(),
        toml::Value::String(config.ai_provider.clone()),
    );
    table.insert(
        "ai_base_url".into(),
        toml::Value::String(config.ai_base_url.clone()),
    );
    table.insert(
        "ai_model".into(),
        toml::Value::String(config.ai_model.clone()),
    );
    table.insert(
        "ai_max_tokens".into(),
        toml::Value::Integer(config.ai_max_tokens as i64),
    );
    table.insert(
        "ai_redact_secrets".into(),
        toml::Value::Boolean(config.ai_redact_secrets),
    );
    table.insert("ai_stream".into(), toml::Value::Boolean(config.ai_stream));
    // Only the file-configured key path is persisted; the JTERM1_AI_API_KEY_FILE
    // override is applied at client construction and never reaches Config.
    match &config.ai_api_key_file {
        Some(path) => {
            table.insert("ai_api_key_file".into(), toml::Value::String(path.clone()));
        }
        None => {
            table.remove("ai_api_key_file");
        }
    }
    table.insert(
        "agent_enabled".into(),
        toml::Value::Boolean(config.agent_enabled),
    );
    table.insert(
        "agent_max_turns".into(),
        toml::Value::Integer(config.agent_max_turns as i64),
    );
    table.insert(
        "notify_long_blocks".into(),
        toml::Value::Boolean(config.notify_long_blocks),
    );
    table.insert(
        "allow_remote_clipboard_write".into(),
        toml::Value::Boolean(config.allow_remote_clipboard_write),
    );

    let mut colors = table
        .remove("colors")
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default();
    colors.insert(
        "foreground".into(),
        toml::Value::String(config::rgba_to_hex(&config.foreground)),
    );
    colors.insert(
        "background".into(),
        toml::Value::String(config::rgba_to_hex(&config.background)),
    );
    colors.insert(
        "cursor".into(),
        toml::Value::String(config::rgba_to_hex(&config.cursor)),
    );
    colors.insert(
        "cursor_foreground".into(),
        toml::Value::String(config::rgba_to_hex(&config.cursor_foreground)),
    );
    table.insert("colors".into(), toml::Value::Table(colors));
}

fn save_config_to_path(
    path: &Path,
    config: &Config,
    expected: Option<&ConfigRevision>,
) -> Result<ConfigRevision, ConfigWriteError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;

    let _lock = ConfigFileLock::acquire(path)?;
    let current = read_optional(path)?;
    let actual_revision = revision_from_content(current.as_deref());
    let Some(expected_revision) = expected else {
        return Err(ConfigWriteError::RevisionUnavailable {
            path: path.to_path_buf(),
        });
    };
    if &actual_revision != expected_revision {
        return Err(ConfigWriteError::Conflict {
            path: path.to_path_buf(),
        });
    }

    let mut table = match current.as_deref() {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(|_| ConfigWriteError::InvalidConfig {
                path: path.to_path_buf(),
                errors: 1,
            })?;
            text.parse::<toml::Table>()
                .map_err(|_| ConfigWriteError::InvalidConfig {
                    path: path.to_path_buf(),
                    errors: 1,
                })?
        }
        None => toml::Table::new(),
    };

    let validation = validate_table(path, &table);
    if validation.errors() > 0 {
        return Err(ConfigWriteError::InvalidConfig {
            path: path.to_path_buf(),
            errors: validation.errors(),
        });
    }

    apply_config_to_table(config, &mut table);
    let mut rendered = table.to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    let rendered = rendered.into_bytes();
    if current.as_deref() == Some(rendered.as_slice()) {
        return Ok(actual_revision);
    }

    let staged = stage_private_file(path, "next", &rendered)?;
    if let Some(current) = current.as_deref() {
        if let Err(error) = rotate_backups(path, current) {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
    }
    if let Err(error) = replace_with_staged(&staged, path) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok(fingerprint(&rendered))
}

pub(crate) fn save_config(
    config: &Config,
    expected: Option<&ConfigRevision>,
) -> Result<ConfigRevision, ConfigWriteError> {
    save_config_to_path(&config::config_file_path(), config, expected)
}

fn valid_backup(path: &Path) -> Result<Option<Vec<u8>>, ConfigWriteError> {
    let Some(bytes) = read_optional(path)? else {
        return Ok(None);
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Ok(None);
    };
    if validate_table(path, &table).errors() > 0 {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn restore_backup_to_path(path: &Path) -> Result<(PathBuf, ConfigRevision), ConfigWriteError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;
    let _lock = ConfigFileLock::acquire(path)?;

    let primary = backup_path_for(path);
    let secondary = secondary_backup_path_for(path);
    let (source, bytes) = if let Some(bytes) = valid_backup(&primary)? {
        (primary, bytes)
    } else if let Some(bytes) = valid_backup(&secondary)? {
        (secondary, bytes)
    } else {
        return Err(ConfigWriteError::BackupUnavailable {
            path: path.to_path_buf(),
        });
    };

    let staged = stage_private_file(path, "restore", &bytes)?;
    if let Some(current) = read_optional(path)? {
        if let Err(error) = atomic_replace(&before_restore_path_for(path), &current) {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
    }
    if let Err(error) = replace_with_staged(&staged, path) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok((source, fingerprint(&bytes)))
}

pub(crate) fn restore_backup() -> Result<(PathBuf, ConfigRevision), ConfigWriteError> {
    restore_backup_to_path(&config::config_file_path())
}

#[derive(Clone, Copy)]
enum ExpectedType {
    Number,
    Integer,
    String,
    Boolean,
    Table,
    Array,
}

impl ExpectedType {
    fn matches(self, value: &toml::Value) -> bool {
        match self {
            Self::Number => value.is_float() || value.is_integer(),
            Self::Integer => value.is_integer(),
            Self::String => value.is_str(),
            Self::Boolean => value.is_bool(),
            Self::Table => value.is_table(),
            Self::Array => value.is_array(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Integer => "integer",
            Self::String => "string",
            Self::Boolean => "boolean",
            Self::Table => "table",
            Self::Array => "array",
        }
    }
}

fn value_kind(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

fn check_type(
    report: &mut ConfigValidationReport,
    table: &toml::Table,
    key: &str,
    expected: ExpectedType,
) {
    let Some(value) = table.get(key) else {
        return;
    };
    if !expected.matches(value) {
        report.error(
            key,
            format!(
                "must be a {}; found {}",
                expected.label(),
                value_kind(value)
            ),
        );
    }
}

fn number_value(table: &toml::Table, key: &str) -> Option<f64> {
    table.get(key).and_then(|value| {
        value
            .as_float()
            .or_else(|| value.as_integer().map(|integer| integer as f64))
    })
}

fn check_number_range(
    report: &mut ConfigValidationReport,
    table: &toml::Table,
    key: &str,
    minimum: f64,
    maximum: f64,
) {
    if let Some(value) = number_value(table, key) {
        if !(minimum..=maximum).contains(&value) {
            report.warning(
                key,
                format!("is outside the supported range {minimum}..={maximum}; it will be clamped"),
            );
        }
    }
}

fn check_integer_range(
    report: &mut ConfigValidationReport,
    table: &toml::Table,
    key: &str,
    minimum: i64,
    maximum: i64,
) {
    if let Some(value) = table.get(key).and_then(toml::Value::as_integer) {
        if !(minimum..=maximum).contains(&value) {
            report.warning(
                key,
                format!("is outside the supported range {minimum}..={maximum}; it will be clamped"),
            );
        }
    }
}

fn check_enum(
    report: &mut ConfigValidationReport,
    table: &toml::Table,
    key: &str,
    accepted: &[&str],
) {
    let Some(value) = table.get(key).and_then(toml::Value::as_str) else {
        return;
    };
    if !accepted
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
    {
        report.error(key, format!("must be one of: {}", accepted.join(", ")));
    }
}

fn check_nonempty_string(report: &mut ConfigValidationReport, table: &toml::Table, key: &str) {
    if table
        .get(key)
        .and_then(toml::Value::as_str)
        .is_some_and(|value| value.trim().is_empty())
    {
        report.error(key, "must not be empty");
    }
}

fn check_absolute_path(report: &mut ConfigValidationReport, table: &toml::Table, key: &str) {
    let Some(value) = table.get(key).and_then(toml::Value::as_str) else {
        return;
    };
    if value.chars().any(char::is_control) {
        report.error(key, "must not contain control characters");
    }
    if value.chars().count() > 16 * 1_024 {
        report.error(key, "must not exceed 16384 characters");
    }
    if !value.trim().is_empty() && !Path::new(value).is_absolute() {
        report.error(key, "must be an absolute path; '~' is not expanded");
    }
}

fn validate_colors(report: &mut ConfigValidationReport, table: &toml::Table) {
    let Some(colors) = table.get("colors").and_then(toml::Value::as_table) else {
        return;
    };
    let known: HashSet<&str> = ["foreground", "background", "cursor", "cursor_foreground"]
        .into_iter()
        .collect();
    for (key, value) in colors {
        let path = format!("colors.{key}");
        if !known.contains(key.as_str()) {
            report.warning(path, "unknown color key; it is preserved but ignored");
            continue;
        }
        let Some(color) = value.as_str() else {
            report.error(path, "must be a color string");
            continue;
        };
        if RGBA::parse(color).is_err() {
            report.error(path, "is not a valid GTK color");
        }
    }
}

fn validate_keybindings(report: &mut ConfigValidationReport, table: &toml::Table) {
    let Some(keybindings) = table.get("keybindings").and_then(toml::Value::as_table) else {
        return;
    };
    let known: HashSet<&str> = Action::all_actions()
        .into_iter()
        .filter_map(|action| action.config_key())
        .collect();
    let mut seen: HashMap<Chord, String> = HashMap::new();
    for (key, value) in keybindings {
        let path = format!("keybindings.{key}");
        if !known.contains(key.as_str()) {
            report.warning(path, "unknown action; the binding is ignored");
            continue;
        }
        // `false` removes a binding; the loader honors it, so it is valid.
        if value.as_bool() == Some(false) {
            continue;
        }
        let Some(binding) = value.as_str() else {
            report.error(path, "must be a shortcut string or false");
            continue;
        };
        // Unbind tokens ("", none, disabled, unbind) are valid values that
        // remove a binding rather than naming a shortcut.
        if is_unbind_token(binding) {
            continue;
        }
        match parse(binding) {
            Ok(chord) => {
                if let Some(previous) = seen.insert(chord, key.clone()) {
                    report.warning(
                        path,
                        format!("duplicates keybindings.{previous}; the later binding wins"),
                    );
                }
            }
            Err(_) => report.error(path, "is not a valid shortcut"),
        }
    }
}

fn validate_remote_text(
    report: &mut ConfigValidationReport,
    path: String,
    value: &str,
    allow_whitespace: bool,
    max_chars: usize,
) {
    if value.chars().any(char::is_control) {
        report.error(path.clone(), "must not contain control characters");
    }
    if !allow_whitespace && value.chars().any(char::is_whitespace) {
        report.error(path.clone(), "must not contain whitespace");
    }
    if value.chars().count() > max_chars {
        report.error(path, format!("must not exceed {max_chars} characters"));
    }
}

fn validate_remote_hosts(report: &mut ConfigValidationReport, table: &toml::Table) {
    let Some(hosts) = table.get("remote_hosts").and_then(toml::Value::as_array) else {
        return;
    };
    let known: HashSet<&str> = [
        "name",
        "host",
        "user",
        "remote_shell",
        "session",
        "ssh_args",
        "login_shell",
        "multiplex",
    ]
    .into_iter()
    .collect();
    let mut names = HashSet::new();
    for (index, value) in hosts.iter().enumerate() {
        let prefix = format!("remote_hosts[{index}]");
        let Some(host) = value.as_table() else {
            report.error(prefix, "must be a table");
            continue;
        };
        for key in host.keys() {
            if !known.contains(key.as_str()) {
                report.warning(
                    format!("{prefix}.{key}"),
                    "unknown remote-host key; it is ignored",
                );
            }
        }
        match host.get("host").and_then(toml::Value::as_str) {
            Some(value) if !value.trim().is_empty() => {
                validate_remote_text(report, format!("{prefix}.host"), value, false, 1_024);
            }
            Some(_) => report.error(format!("{prefix}.host"), "must not be empty"),
            None => report.error(format!("{prefix}.host"), "is required and must be a string"),
        }
        for key in ["name", "user", "remote_shell", "session"] {
            if let Some(value) = host.get(key) {
                if let Some(value) = value.as_str() {
                    if value.trim().is_empty() {
                        report.error(format!("{prefix}.{key}"), "must not be empty");
                    } else {
                        let (allow_whitespace, max_chars) = match key {
                            "name" => (true, 256),
                            "remote_shell" => (true, 16 * 1_024),
                            "session" => (true, 1_024),
                            _ => (false, 256),
                        };
                        validate_remote_text(
                            report,
                            format!("{prefix}.{key}"),
                            value,
                            allow_whitespace,
                            max_chars,
                        );
                    }
                } else {
                    report.error(format!("{prefix}.{key}"), "must be a string");
                }
            }
        }
        for key in ["login_shell", "multiplex"] {
            if let Some(value) = host.get(key) {
                if !value.is_bool() {
                    report.error(format!("{prefix}.{key}"), "must be a boolean");
                }
            }
        }
        if let Some(value) = host.get("ssh_args") {
            match value.as_array() {
                Some(arguments) if arguments.iter().all(toml::Value::is_str) => {
                    if arguments.len() > 128 {
                        report.error(
                            format!("{prefix}.ssh_args"),
                            "must not contain more than 128 arguments",
                        );
                    }
                    for (argument_index, argument) in arguments.iter().enumerate() {
                        validate_remote_text(
                            report,
                            format!("{prefix}.ssh_args[{argument_index}]"),
                            argument.as_str().unwrap_or_default(),
                            true,
                            16 * 1_024,
                        );
                    }
                }
                Some(_) => report.error(format!("{prefix}.ssh_args"), "must contain only strings"),
                None => report.error(format!("{prefix}.ssh_args"), "must be an array"),
            }
        }
        let effective_name = host
            .get("name")
            .and_then(toml::Value::as_str)
            .map(|name| ("name", name))
            .or_else(|| {
                host.get("host")
                    .and_then(toml::Value::as_str)
                    .map(|name| ("host", name))
            });
        if let Some((key, name)) = effective_name {
            if !name.trim().is_empty() && !names.insert(name.to_string()) {
                report.error(
                    format!("{prefix}.{key}"),
                    "must be unique because session restore uses it as the profile identifier",
                );
            }
        }
    }
}

fn validate_table(path: &Path, table: &toml::Table) -> ConfigValidationReport {
    let mut report = ConfigValidationReport::new(path, true);
    let known: HashSet<&str> = [
        "opacity",
        "scrollback",
        "font",
        "font_scale",
        "theme",
        "colors",
        "keybindings",
        "shell",
        "startup_commands",
        "terminal_mode",
        "tab_placement",
        "sidebar_view",
        "jsh_update_check",
        "sidebar_visible",
        "sidebar_width",
        "tab_width",
        "ansi_cache_capacity",
        "max_visible_blocks",
        "output_batch_min_ms",
        "output_batch_max_ms",
        "lazy_load_threshold",
        "truncation_threshold_lines",
        "finished_block_viewport_rows",
        "finished_block_max_expanded_rows",
        "max_collapsed_output_lines",
        "virtual_scroll_margin",
        "command_history_enabled",
        "command_history_path",
        "command_history_max_entries",
        "block_history_path",
        "block_history_compress",
        "block_compact",
        "editor_input",
        "allow_remote_clipboard_write",
        "ai_enabled",
        "ai_provider",
        "ai_base_url",
        "ai_model",
        "ai_max_tokens",
        "ai_temperature",
        "ai_redact_secrets",
        "ai_stream",
        "ai_api_key_file",
        "agent_enabled",
        "agent_max_turns",
        "mouse_reporting_enabled",
        "focus_reporting_enabled",
        "scroll_reporting_enabled",
        "preserve_live_scrollback",
        "notify_long_blocks",
        "notify_long_block_threshold_ms",
        "show_repo_strip",
        "remote_hosts",
    ]
    .into_iter()
    .collect();
    for key in table.keys() {
        if !known.contains(key.as_str()) {
            report.warning(
                key,
                "unknown key; it is preserved but ignored by this version",
            );
        }
    }

    for key in ["opacity", "font_scale"] {
        check_type(&mut report, table, key, ExpectedType::Number);
    }
    for key in [
        "scrollback",
        "sidebar_width",
        "tab_width",
        "ansi_cache_capacity",
        "max_visible_blocks",
        "output_batch_min_ms",
        "output_batch_max_ms",
        "lazy_load_threshold",
        "truncation_threshold_lines",
        "finished_block_viewport_rows",
        "finished_block_max_expanded_rows",
        "max_collapsed_output_lines",
        "virtual_scroll_margin",
        "command_history_max_entries",
        "ai_max_tokens",
        "agent_max_turns",
        "notify_long_block_threshold_ms",
    ] {
        check_type(&mut report, table, key, ExpectedType::Integer);
    }
    for key in [
        "font",
        "theme",
        "shell",
        "startup_commands",
        "terminal_mode",
        "tab_placement",
        "sidebar_view",
        "command_history_path",
        "block_history_path",
        "ai_provider",
        "ai_base_url",
        "ai_model",
    ] {
        check_type(&mut report, table, key, ExpectedType::String);
    }
    for key in [
        "command_history_enabled",
        "block_history_compress",
        "block_compact",
        "editor_input",
        "allow_remote_clipboard_write",
        "sidebar_visible",
        "ai_enabled",
        "ai_redact_secrets",
        "ai_stream",
        "agent_enabled",
        "mouse_reporting_enabled",
        "focus_reporting_enabled",
        "scroll_reporting_enabled",
        "preserve_live_scrollback",
        "notify_long_blocks",
        "show_repo_strip",
    ] {
        check_type(&mut report, table, key, ExpectedType::Boolean);
    }
    check_type(&mut report, table, "colors", ExpectedType::Table);
    check_type(&mut report, table, "keybindings", ExpectedType::Table);
    check_type(&mut report, table, "remote_hosts", ExpectedType::Array);

    check_nonempty_string(&mut report, table, "font");
    check_nonempty_string(&mut report, table, "theme");
    check_nonempty_string(&mut report, table, "shell");
    check_nonempty_string(&mut report, table, "ai_provider");
    check_nonempty_string(&mut report, table, "ai_base_url");
    check_nonempty_string(&mut report, table, "ai_model");
    check_absolute_path(&mut report, table, "shell");
    check_enum(
        &mut report,
        table,
        "theme",
        &[
            "default",
            "light",
            "solarized-dark",
            "solarized-light",
            "gruvbox-dark",
            "gruvbox-light",
            "dracula",
            "nord",
        ],
    );
    check_enum(&mut report, table, "terminal_mode", &["block", "vte"]);
    check_enum(
        &mut report,
        table,
        "ai_provider",
        &[
            "anthropic",
            "claude",
            "openai",
            "openai-compatible",
            "openai_compatible",
            "ollama",
        ],
    );
    check_enum(
        &mut report,
        table,
        "tab_placement",
        &["sidebar", "top", "topbar", "top_bar"],
    );
    check_enum(
        &mut report,
        table,
        "sidebar_view",
        &["tabs", "files", "file", "filetree", "file_tree"],
    );
    check_absolute_path(&mut report, table, "command_history_path");
    check_absolute_path(&mut report, table, "block_history_path");

    if let Some(url) = table.get("ai_base_url").and_then(toml::Value::as_str) {
        let url = url.trim();
        let valid = (url.starts_with("http://") || url.starts_with("https://"))
            && url
                .split_once("://")
                .is_some_and(|(_, authority)| !authority.is_empty())
            && !url.chars().any(char::is_whitespace);
        if !valid {
            report.error(
                "ai_base_url",
                "must be an absolute http(s) URL without whitespace",
            );
        }
    }

    check_number_range(&mut report, table, "opacity", 0.01, 1.0);
    check_number_range(&mut report, table, "font_scale", 0.1, 10.0);
    check_integer_range(&mut report, table, "scrollback", 0, 1_000_000);
    check_integer_range(&mut report, table, "sidebar_width", 120, 800);
    check_integer_range(&mut report, table, "tab_width", 80, 480);
    check_integer_range(&mut report, table, "ansi_cache_capacity", 1, 65_536);
    check_integer_range(&mut report, table, "max_visible_blocks", 1, 10_000);
    check_integer_range(&mut report, table, "output_batch_min_ms", 1, 1_000);
    check_integer_range(&mut report, table, "output_batch_max_ms", 1, 5_000);
    check_integer_range(&mut report, table, "lazy_load_threshold", 1, 100_000);
    check_integer_range(
        &mut report,
        table,
        "truncation_threshold_lines",
        100,
        1_000_000,
    );
    check_integer_range(&mut report, table, "finished_block_viewport_rows", 3, 5_000);
    check_integer_range(
        &mut report,
        table,
        "finished_block_max_expanded_rows",
        3,
        5_000,
    );
    check_integer_range(&mut report, table, "max_collapsed_output_lines", 0, 10_000);
    check_integer_range(&mut report, table, "virtual_scroll_margin", 0, 100);
    check_integer_range(
        &mut report,
        table,
        "command_history_max_entries",
        100,
        100_000,
    );
    check_integer_range(&mut report, table, "ai_max_tokens", 1, 32_768);
    check_integer_range(&mut report, table, "agent_max_turns", 1, 100);
    check_integer_range(
        &mut report,
        table,
        "notify_long_block_threshold_ms",
        0,
        i64::MAX,
    );

    if let (Some(minimum), Some(maximum)) = (
        table
            .get("output_batch_min_ms")
            .and_then(toml::Value::as_integer),
        table
            .get("output_batch_max_ms")
            .and_then(toml::Value::as_integer),
    ) {
        if maximum < minimum {
            report.error(
                "output_batch_max_ms",
                "must be greater than or equal to output_batch_min_ms",
            );
        }
    }
    if let (Some(viewport), Some(expanded)) = (
        table
            .get("finished_block_viewport_rows")
            .and_then(toml::Value::as_integer),
        table
            .get("finished_block_max_expanded_rows")
            .and_then(toml::Value::as_integer),
    ) {
        if expanded < viewport {
            report.error(
                "finished_block_max_expanded_rows",
                "must be greater than or equal to finished_block_viewport_rows",
            );
        }
    }

    validate_colors(&mut report, table);
    validate_keybindings(&mut report, table);
    validate_remote_hosts(&mut report, table);
    report
}

fn validate_path_with_missing_policy(
    path: &Path,
    missing_is_error: bool,
) -> ConfigValidationReport {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut report = ConfigValidationReport::new(path, false);
            if missing_is_error {
                report.error("config", "file does not exist");
            } else {
                report.warning(
                    "config",
                    "file does not exist; built-in defaults will be used",
                );
            }
            return report;
        }
        Err(_) => {
            let mut report = ConfigValidationReport::new(path, true);
            report.error("config", "file cannot be read");
            return report;
        }
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            let mut report = ConfigValidationReport::new(path, true);
            report.error("config", "file is not valid UTF-8");
            return report;
        }
    };
    let table = match text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(_) => {
            let mut report = ConfigValidationReport::new(path, true);
            report.error("config", "file is not valid TOML");
            return report;
        }
    };
    validate_table(path, &table)
}

pub(crate) fn validate_path(path: &Path) -> ConfigValidationReport {
    validate_path_with_missing_policy(path, false)
}

pub(crate) fn validate_current_config() -> ConfigValidationReport {
    validate_path(&config::config_file_path())
}

fn print_validation_human(report: &ConfigValidationReport) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "jterm1 configuration check")?;
    writeln!(stdout, "path: {}\n", report.path)?;
    if report.issues.is_empty() {
        writeln!(stdout, "[ok   ] configuration is valid")?;
    } else {
        for issue in &report.issues {
            writeln!(
                stdout,
                "[{:<5}] {}: {}",
                issue.severity.label(),
                issue.key,
                issue.message
            )?;
        }
    }
    writeln!(
        stdout,
        "\nSummary: {} error(s), {} warning(s)",
        report.errors, report.warnings
    )
}

fn print_validation_json(report: &ConfigValidationReport) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, report).map_err(io::Error::other)?;
    writeln!(stdout)
}

pub(crate) fn run_check_path(path: &Path, format: ReportFormat) -> bool {
    let report = validate_path_with_missing_policy(path, true);
    let result = match format {
        ReportFormat::Human => print_validation_human(&report),
        ReportFormat::Json => print_validation_json(&report),
    };
    if let Err(error) = result {
        eprintln!("jterm1: failed to write configuration report: {error}");
        return false;
    }
    report.healthy()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "jterm1-config-store-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn explicit_missing_config_is_an_error_but_default_discovery_is_a_warning() {
        let path = temporary_directory("missing-check").join("absent.toml");
        let explicit = validate_path_with_missing_policy(&path, true);
        assert!(!explicit.exists());
        assert_eq!(explicit.errors(), 1);
        assert!(!explicit.healthy());

        let discovered = validate_path(&path);
        assert!(!discovered.exists());
        assert_eq!(discovered.errors(), 0);
        assert_eq!(discovered.warnings(), 1);
        assert!(discovered.healthy());
    }

    #[test]
    fn revisions_detect_external_changes() {
        let directory = temporary_directory("revision");
        let path = directory.join("config.toml");
        fs::write(&path, "opacity = 0.5\n").unwrap();
        let first = revision_at(&path).unwrap();
        fs::write(&path, "opacity = 0.6\n").unwrap();
        let second = revision_at(&path).unwrap();
        assert_ne!(first, second);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_writer_is_rejected_without_touching_disk() {
        let directory = temporary_directory("conflict");
        let path = directory.join("config.toml");
        fs::write(&path, "opacity = 0.5\n").unwrap();
        let expected = revision_at(&path).unwrap();
        fs::write(&path, "opacity = 0.6\n").unwrap();
        let config = config::load_config().0;
        let error = save_config_to_path(&path, &config, Some(&expected)).unwrap_err();
        assert!(error.is_conflict());
        assert_eq!(fs::read_to_string(&path).unwrap(), "opacity = 0.6\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ai_provider_settings_are_persisted_without_credentials() {
        let directory = temporary_directory("ai-provider");
        let path = directory.join("config.toml");
        let mut config = config::load_safe_config().0;
        config.ai_provider = "ollama".into();
        config.ai_base_url = "http://localhost:11434".into();
        config.ai_model = "qwen2.5-coder:7b".into();
        config.ai_max_tokens = 2_048;
        config.ai_redact_secrets = false;
        config.ai_stream = false;

        save_config_to_path(&path, &config, Some(&ConfigRevision::Missing)).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        let table = contents.parse::<toml::Table>().unwrap();
        assert_eq!(
            table.get("ai_provider").and_then(toml::Value::as_str),
            Some("ollama")
        );
        assert_eq!(
            table.get("ai_base_url").and_then(toml::Value::as_str),
            Some("http://localhost:11434")
        );
        assert_eq!(
            table.get("ai_model").and_then(toml::Value::as_str),
            Some("qwen2.5-coder:7b")
        );
        assert_eq!(
            table.get("ai_max_tokens").and_then(toml::Value::as_integer),
            Some(2_048)
        );
        assert_eq!(
            table
                .get("ai_redact_secrets")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            table.get("ai_stream").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert!(!contents.to_ascii_lowercase().contains("api_key"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lock_status_tracks_an_active_writer_and_clears_after_drop() {
        let directory = temporary_directory("lock");
        let path = directory.join("config.toml");
        let guard = ConfigFileLock::acquire(&path).unwrap();
        let lock_path = lock_path_for(&path);
        let observer = open_lock_file(&lock_path).unwrap();
        assert!(!try_lock_exclusive(&observer).unwrap());
        drop(observer);
        drop(guard);
        let observer = open_lock_file(&lock_path).unwrap();
        assert!(try_lock_exclusive(&observer).unwrap());
        unlock(&observer);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn validation_reports_unknown_and_invalid_keys_without_values() {
        let directory = temporary_directory("validation");
        let path = directory.join("config.toml");
        fs::write(
            &path,
            "opacity = 'secret-value'\nunknown_setting = 'also-secret'\n",
        )
        .unwrap();
        let report = validate_path(&path);
        assert_eq!(report.errors(), 1);
        assert_eq!(report.warnings(), 1);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret-value"));
        assert!(!json.contains("also-secret"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn validation_rejects_unsafe_remote_fields_without_echoing_them() {
        let directory = temporary_directory("remote-validation");
        let path = directory.join("config.toml");
        fs::write(
            &path,
            concat!(
                "[[remote_hosts]]\n",
                "name = 'staging'\n",
                "host = 'bad host'\n",
                "user = 'bad user'\n",
                "remote_shell = ''\n",
                "session = \"prod\\tsecret\"\n",
                "ssh_args = [\"-p\", \"22\\tProxyCommand=secret\"]\n",
            ),
        )
        .unwrap();
        let report = validate_path(&path);
        let json = serde_json::to_string(&report).unwrap();
        for key in [
            "remote_hosts[0].host",
            "remote_hosts[0].user",
            "remote_hosts[0].remote_shell",
            "remote_hosts[0].session",
            "remote_hosts[0].ssh_args[1]",
        ] {
            assert!(json.contains(key), "missing validation issue for {key}");
        }
        assert!(!json.contains("ProxyCommand"));
        assert!(!json.contains("secret"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn validation_rejects_duplicate_remote_profile_names() {
        let table = concat!(
            "[[remote_hosts]]\n",
            "name = 'shared-name'\n",
            "host = 'first.example'\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'shared-name'\n",
            "host = 'second.example'\n",
        )
        .parse::<toml::Table>()
        .unwrap();

        let report = validate_table(Path::new("config.toml"), &table);
        assert_eq!(report.errors(), 1);
        assert!(report.issues.iter().any(|issue| {
            issue.key == "remote_hosts[1].name" && issue.message.contains("profile identifier")
        }));

        let implicit = concat!(
            "[[remote_hosts]]\n",
            "host = 'same.example'\n",
            "\n",
            "[[remote_hosts]]\n",
            "host = 'same.example'\n",
        )
        .parse::<toml::Table>()
        .unwrap();
        let report = validate_table(Path::new("config.toml"), &implicit);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.key == "remote_hosts[1].host"));
    }

    #[test]
    fn validation_checks_ai_provider_settings() {
        let table = "ai_provider = 'mystery'\nai_base_url = 'file:///tmp/model'\nai_model = ''\nai_max_tokens = 999999\nai_redact_secrets = 'yes'\n"
            .parse::<toml::Table>()
            .unwrap();
        let report = validate_table(Path::new("config.toml"), &table);
        for key in [
            "ai_provider",
            "ai_base_url",
            "ai_model",
            "ai_max_tokens",
            "ai_redact_secrets",
        ] {
            assert!(
                report.issues.iter().any(|issue| issue.key == key),
                "missing validation issue for {key}: {:?}",
                report.issues
            );
        }

        let aliases = "ai_provider = 'openai_compatible'\nai_base_url = 'http://localhost:8000/v1'\nai_model = 'local-model'\nai_max_tokens = 1\nai_redact_secrets = true\n"
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(
            validate_table(Path::new("config.toml"), &aliases).errors(),
            0
        );
    }

    #[test]
    fn validation_checks_sidebar_visibility_and_agent_turn_cap() {
        let table = "sidebar_visible = 'yes'\nagent_max_turns = 101\n"
            .parse::<toml::Table>()
            .unwrap();
        let report = validate_table(Path::new("config.toml"), &table);
        for key in ["sidebar_visible", "agent_max_turns"] {
            assert!(
                report.issues.iter().any(|issue| issue.key == key),
                "missing validation issue for {key}: {:?}",
                report.issues
            );
        }
    }

    #[test]
    fn restore_uses_secondary_when_primary_is_invalid() {
        let directory = temporary_directory("restore");
        let path = directory.join("config.toml");
        fs::write(&path, "not valid toml = [\n").unwrap();
        fs::write(backup_path_for(&path), "also invalid = [\n").unwrap();
        fs::write(secondary_backup_path_for(&path), "opacity = 0.7\n").unwrap();
        let (source, _) = restore_backup_to_path(&path).unwrap();
        assert_eq!(source, secondary_backup_path_for(&path));
        assert_eq!(fs::read_to_string(&path).unwrap(), "opacity = 0.7\n");
        assert!(before_restore_path_for(&path).is_file());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn revision_debug_output_never_contains_configuration_bytes() {
        let revision = fingerprint(b"api_key = 'secret-value'");
        let debug = format!("{revision:?}");
        assert!(debug.contains("bytes"));
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn rotating_backups_keep_the_previous_two_valid_versions() {
        let directory = temporary_directory("rotation");
        let path = directory.join("config.toml");
        fs::write(&path, "opacity = 0.4\n").unwrap();
        rotate_backups(&path, b"opacity = 0.4\n").unwrap();
        rotate_backups(&path, b"opacity = 0.5\n").unwrap();
        assert_eq!(
            fs::read_to_string(backup_path_for(&path)).unwrap(),
            "opacity = 0.5\n"
        );
        assert_eq!(
            fs::read_to_string(secondary_backup_path_for(&path)).unwrap(),
            "opacity = 0.4\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn documented_example_passes_schema_validation() {
        let table = include_str!("../config.toml.example")
            .parse::<toml::Table>()
            .unwrap();
        let report = validate_table(Path::new("config.toml"), &table);
        assert_eq!(report.errors(), 0);
    }
}
