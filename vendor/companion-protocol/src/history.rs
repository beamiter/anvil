//! Explicit, scope-bound dialogue history pages. A page is a read-only view:
//! it never restores audio, execution rights, consent or model context.
use crate::{identifier, valid_context, ContextRef};
use serde::{Deserialize, Serialize};

/// Entries per reply page.
pub const MAX_PAGE: usize = 8;
/// Serialized entry bytes per page; the whole reply stays below `MAX_FRAME`.
pub const MAX_PAGE_BYTES: usize = 48 * 1024;
/// Entries one host view keeps after paging.
pub const MAX_VIEW: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The exact host, pane, task and project of the request.
    #[default]
    Context,
    /// The current task inside the current project, from any host.
    Task,
    /// The current project, from any host.
    Project,
    /// Eva voice turns. They have no pane and retain no transcript.
    Voice,
}
impl Scope {
    pub fn available(self, context: &ContextRef) -> bool {
        match self {
            Self::Task => context.task_id.is_some(),
            Self::Project => context.project_id.is_some(),
            Self::Context | Self::Voice => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Text,
    Offline,
    Voice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum End {
    ResponseComplete,
    Interrupted,
    NextVoiceTurn,
    AudioStopped,
    Error,
    ObservationLost,
    ResourcesReleased,
    RestartedUnknown,
}

/// Why dialogue text is or is not part of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Text {
    /// Session-only text from the running coordinator.
    Shown,
    /// Text lived only in the memory of an earlier coordinator session.
    EarlierSession,
    /// No text was recorded. Voice transcripts are never kept.
    NotRecorded,
    /// The turn used reviewed terminal text; only its original pane may read it.
    WithheldOutsideOrigin,
    /// The text alone would exceed one page.
    TooLarge,
}

/// Latest recorded operation receipts of one voice turn. Unsubmitted plan
/// steps are not counted and are never implied complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipts {
    pub tool_calls: u32,
    pub succeeded: u32,
    pub partial: u32,
    pub failed_or_cancelled: u32,
    pub unknown: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub cursor: String,
    pub turn_id: String,
    pub created_at_ms: u64,
    pub route: Route,
    /// Original text context. Voice turns have none.
    pub source: Option<ContextRef>,
    pub text: Text,
    pub input: Option<String>,
    pub reply: Option<String>,
    pub generated: bool,
    /// A host reported showing the reply. It is not proof that it was read.
    pub displayed: bool,
    pub interrupted: bool,
    pub finished: bool,
    pub end: Option<End>,
    pub recovered_after_restart: bool,
    /// Raw counters; no sample rate or duration is inferred.
    pub generated_samples: Option<u64>,
    pub confirmed_played_samples: Option<u64>,
    pub receipts: Option<Receipts>,
}
impl Entry {
    pub fn valid(&self) -> bool {
        cursor_valid(&self.cursor)
            && identifier(&self.turn_id)
            && self.created_at_ms > 0
            && self.source.as_ref().is_none_or(valid_context)
            && (self.route == Route::Voice) == self.source.is_none()
            && ((self.text == Text::Shown) || (self.input.is_none() && self.reply.is_none()))
            && self.input.as_ref().is_none_or(|t| t.len() <= 8192)
            && self.reply.as_ref().is_none_or(|t| t.len() <= 8192)
            && match (self.generated_samples, self.confirmed_played_samples) {
                (Some(generated), Some(played)) => played <= generated,
                (None, None) => true,
                _ => false,
            }
    }
}

pub fn cursor_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Statuses a coordinator may return. Anything else is treated as unavailable.
pub const STATUSES: [&str; 7] = [
    "ok",
    "scope_unavailable",
    "stale_context",
    "history_cursor_stale",
    "desktop_locked_or_unknown",
    "history_unavailable",
    "history_not_negotiated",
];

#[cfg(test)]
mod tests {
    use super::*;
    fn entry() -> Entry {
        Entry {
            cursor: "a".repeat(64),
            turn_id: "turn".into(),
            created_at_ms: 1,
            route: Route::Offline,
            source: Some(ContextRef {
                host_id: "host".into(),
                pane_id: "pane".into(),
                task_id: None,
                project_id: None,
                revision: 1,
            }),
            text: Text::Shown,
            input: Some("hello".into()),
            reply: Some("hi".into()),
            generated: true,
            displayed: false,
            interrupted: false,
            finished: true,
            end: Some(End::ResponseComplete),
            recovered_after_restart: false,
            generated_samples: None,
            confirmed_played_samples: None,
            receipts: None,
        }
    }
    #[test]
    fn cursors_are_lowercase_sha256_hex_only() {
        assert!(cursor_valid(&"0f".repeat(32)));
        assert!(!cursor_valid(&"0F".repeat(32)));
        assert!(!cursor_valid(&"0f".repeat(31)));
        assert!(!cursor_valid(&format!("{}g", "0".repeat(63))));
    }
    #[test]
    fn withheld_or_voice_entries_cannot_carry_text_or_inconsistent_counters() {
        assert!(entry().valid());
        let mut withheld = entry();
        withheld.text = Text::WithheldOutsideOrigin;
        assert!(!withheld.valid());
        withheld.input = None;
        withheld.reply = None;
        assert!(withheld.valid());
        let mut voice = entry();
        voice.route = Route::Voice;
        assert!(!voice.valid(), "voice entries have no pane source");
        voice.source = None;
        voice.text = Text::NotRecorded;
        voice.input = None;
        voice.reply = None;
        voice.generated_samples = Some(10);
        assert!(!voice.valid());
        voice.confirmed_played_samples = Some(11);
        assert!(!voice.valid());
        voice.confirmed_played_samples = Some(10);
        assert!(voice.valid());
    }
    #[test]
    fn task_and_project_scopes_need_the_current_identity() {
        let mut context = entry().source.unwrap();
        assert!(Scope::Context.available(&context) && Scope::Voice.available(&context));
        assert!(!Scope::Task.available(&context) && !Scope::Project.available(&context));
        context.task_id = Some("task".into());
        context.project_id = Some("project".into());
        assert!(Scope::Task.available(&context) && Scope::Project.available(&context));
    }
}
