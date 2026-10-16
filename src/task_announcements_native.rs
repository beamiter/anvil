//! Owned-display fixture: synthetic task data only, no providers or PTYs.
//! Python AT-SPI/Orca observer drives explicit file handshakes for each phase.
use super::*;
use crate::agent_task::*;
use crate::agent_task_ui;
use adw::prelude::AdwApplicationWindowExt;
use gtk::prelude::*;
use relm4::{adw, gtk};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn pump() {
    while gtk::glib::MainContext::default().iteration(false) {}
}
fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "accessibility observer did not acknowledge {}",
            path.display()
        );
        pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn atomic_json(path: &Path, value: serde_json::Value) {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec(&value).unwrap()).unwrap();
    std::fs::rename(temp, path).unwrap();
}

struct Fixture {
    dir: PathBuf,
    window: adw::ApplicationWindow,
    input: gtk::Entry,
    owner: gtk::Widget,
    label: gtk::Label,
    tasks: TaskManager,
    state: TaskAnnouncements,
    phase: usize,
}
impl Fixture {
    fn observe(&mut self) -> Vec<(TaskId, String)> {
        self.state.observe(
            self.tasks
                .tasks()
                .iter()
                .map(|task| TaskAnnouncementSnapshot {
                    id: task.id,
                    status: task.status,
                    terminal: task.terminal_session_id.clone(),
                    validation_attempt: task.validation.attempt,
                    validation_status: task.validation.status,
                }),
        )
    }
    fn phase(
        &mut self,
        name: &str,
        completions: &[AgentRuntimeCompletion],
        issues: &[AgentRuntimeIssue],
        expected: &[(TaskId, &str)],
    ) {
        let mut messages = self.observe();
        self.state.sessions_finished(completions, &mut messages);
        let details = agent_task_ui::merge_task_issue_feedback(&mut messages, issues);
        let expected_statuses: Vec<_> = expected
            .iter()
            .map(|(id, status)| (*id, status.to_string()))
            .collect();
        assert_eq!(messages, expected_statuses, "semantic oracle: {name}");
        let expected_text: Vec<_> = expected
            .iter()
            .map(|(id, status)| {
                format!(
                    "{}: {status}",
                    agent_task_ui::feedback_identity(&self.tasks, *id)
                )
            })
            .collect();
        let last_visible = messages.last().map(|(id, status)| {
            let text = format!(
                "{}: {status}",
                agent_task_ui::feedback_identity(&self.tasks, *id)
            );
            details
                .get(id)
                .map_or(text.clone(), |detail| format!("{text}. {detail}"))
        });
        let prefix = format!("{:02}", self.phase);
        atomic_json(
            &self.dir.join(format!("{prefix}.ready.json")),
            serde_json::json!({
                "phase":self.phase, "name":name, "pid":std::process::id(),
                "window":self.window.title().unwrap().as_str(), "expected":expected_text,
                "visible":last_visible, "focus":"A11 draft owner",
            }),
        );
        wait_file(&self.dir.join(format!("{prefix}.go")));
        for (id, status) in messages {
            let message = format!(
                "{}: {status}",
                agent_task_ui::feedback_identity(&self.tasks, id)
            );
            agent_task_ui::deliver_task_feedback(
                self.window.upcast_ref(),
                &self.label,
                &message,
                details.get(&id).map(String::as_str),
            );
        }
        self.state.retain_issue_details(details);
        pump();
        assert_eq!(
            gtk::prelude::GtkWindowExt::focus(&self.window),
            Some(self.owner.clone())
        );
        assert_eq!(self.input.text(), "unfinished synthetic draft");
        if let Some(visible) = last_visible {
            assert_eq!(self.label.text(), visible);
            assert!(self.label.is_mapped());
        }
        atomic_json(
            &self.dir.join(format!("{prefix}.done.json")),
            serde_json::json!({"focus_preserved":true,"draft_preserved":true}),
        );
        wait_file(&self.dir.join(format!("{prefix}.ack")));
        self.phase += 1;
    }
    fn apply(&mut self, stream: &AgentEventStream, sequence: u64, kind: AgentEventKind) {
        let boundary = match kind {
            AgentEventKind::TurnStarted { turn_id } => Some((turn_id, false)),
            AgentEventKind::TurnCompleted { turn_id } => Some((turn_id, true)),
            _ => None,
        };
        let status = self
            .tasks
            .apply_agent_event(stream.event(sequence, kind, None))
            .unwrap();
        if let Some((turn_id, completed)) = boundary {
            self.state.applied_turns(&[AppliedAgentTurn {
                task_id: stream.task_id(),
                epoch: stream.epoch(),
                turn_id,
                completed,
                status,
            }]);
        }
    }
}
fn completion(id: TaskId, outcome: AgentSessionOutcome) -> AgentRuntimeCompletion {
    AgentRuntimeCompletion {
        task_id: id,
        outcome,
        cause: CodexAppServerExitCause::IoFailed,
        detail: None,
    }
}

#[test]
#[ignore = "requires owned Xvfb/D-Bus/AT-SPI/Orca observer; never run on a personal desktop"]
fn semantic_task_results_reach_atspi_and_orca() {
    assert_eq!(std::env::var("ANVIL_A11_OWNED_SESSION").as_deref(), Ok("1"));
    let dir =
        PathBuf::from(std::env::var_os("ANVIL_A11_EVIDENCE").expect("observer evidence directory"));
    assert!(dir.is_dir());
    let token = std::env::var("ANVIL_A11_TOKEN").expect("unique fixture token");
    assert!(token.len() == 16 && token.bytes().all(|c| c.is_ascii_hexdigit()));
    adw::init().unwrap();
    let app = adw::Application::builder()
        .application_id("dev.anvil.A11Fixture")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(gtk::gio::Cancellable::NONE).unwrap();
    let window = adw::ApplicationWindow::new(&app);
    window.set_title(Some(&format!("A11 fixture {token}")));
    window.set_default_size(900, 600);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let input = gtk::Entry::new();
    input.set_text("unfinished synthetic draft");
    input.update_property(&[gtk::accessible::Property::Label("A11 draft owner")]);
    content.append(&input);
    // Exercise the actual Tasks panel component's mapped/closed lifetime. Its
    // model is intentionally empty; task lifecycle remains application-owned.
    use relm4::{Component, ComponentController};
    let panel = crate::dialogs::tasks_panel::TasksPanelModel::builder()
        .launch(())
        .detach();
    content.append(panel.widget());
    let label = agent_task_ui::build_task_feedback();
    content.append(&label);
    window.set_content(Some(&content));
    window.present();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !window.is_mapped() {
        assert!(Instant::now() < deadline);
        pump();
    }
    assert!(input.grab_focus());
    pump();
    let owner = gtk::prelude::GtkWindowExt::focus(&window).unwrap();
    let mut tasks = TaskManager::new();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(
            tasks
                .create(NewTask {
                    title: format!("A11 {token} same task"),
                    provider: AgentProvider::Codex,
                    repo_root: "/synthetic".into(),
                    worktree_path: "/synthetic/task".into(),
                    branch: "fixture/a11".into(),
                    base_commit: "a".repeat(40),
                    source_context: None,
                })
                .unwrap(),
        );
    }
    assert_ne!(
        agent_task_ui::feedback_identity(&tasks, ids[0]),
        agent_task_ui::feedback_identity(&tasks, ids[1])
    );
    tasks
        .update_status(ids[4], TaskStatus::Failed, None)
        .unwrap();
    let mut f = Fixture {
        dir,
        window,
        input,
        owner,
        label,
        tasks,
        state: TaskAnnouncements::default(),
        phase: 0,
    };
    f.phase("initial-history-seed", &[], &[], &[]);
    let a = ids[0];
    let stream = f.tasks.start_agent_event_stream(a).unwrap();
    f.apply(
        &stream,
        1,
        AgentEventKind::SessionStarted {
            provider_session_id: None,
            resumed: false,
        },
    );
    let turn = AgentTurnId::new();
    f.apply(&stream, 2, AgentEventKind::TurnStarted { turn_id: turn });
    f.apply(&stream, 3, AgentEventKind::TurnCompleted { turn_id: turn });
    f.phase(
        "same-poll-start-complete",
        &[],
        &[],
        &[(a, "Ready for review")],
    );
    f.phase("unchanged-poll", &[], &[], &[]);
    panel.widget().set_visible(false);
    f.phase("closed-panel-no-replay", &[], &[], &[]);
    let turn = AgentTurnId::new();
    f.apply(&stream, 4, AgentEventKind::TurnStarted { turn_id: turn });
    for seq in 5..69 {
        f.apply(&stream, seq, AgentEventKind::TextDelta);
    }
    f.phase("closed-panel-token-drain", &[], &[], &[]);
    f.apply(&stream, 69, AgentEventKind::TurnCompleted { turn_id: turn });
    f.phase(
        "closed-panel-new-turn-complete",
        &[],
        &[],
        &[(a, "Ready for review")],
    );
    panel.widget().set_visible(true);
    f.phase("reopened-panel-no-replay", &[], &[], &[]);
    f.tasks
        .finish_agent_event_stream_after_stop(&stream, AgentSessionOutcome::Clean, None)
        .unwrap();
    f.phase(
        "native-session-stop",
        &[completion(a, AgentSessionOutcome::Clean)],
        &[],
        &[(a, "Agent session stopped cleanly")],
    );
    for (attempt, exit, status) in [
        (1, 7, "Validation #1 failed"),
        (2, 0, "Validation #2 passed"),
    ] {
        let session = format!("fixture-validation-{attempt}");
        f.tasks.bind_validation_session(a, session.clone()).unwrap();
        f.phase("validation-running", &[], &[], &[]);
        f.tasks.handle_terminal_session_exit(&session, Some(exit));
        f.tasks.handle_terminal_session_closed(&session);
        f.phase("validation-result", &[], &[], &[(a, status)]);
        f.tasks.handle_terminal_session_closed(&session);
        f.phase("validation-close-no-replay", &[], &[], &[]);
    }
    f.tasks.complete_after_validation(a).unwrap();
    f.phase("task-complete", &[], &[], &[(a, "Task completed")]);
    for (id, outcome, text) in [
        (
            ids[1],
            AgentSessionOutcome::Failed,
            "Task failed; open Tasks for details",
        ),
        (ids[2], AgentSessionOutcome::Cancelled, "Task cancelled"),
    ] {
        let stream = f.tasks.start_agent_event_stream(id).unwrap();
        let turn = AgentTurnId::new();
        f.apply(
            &stream,
            1,
            AgentEventKind::SessionStarted {
                provider_session_id: None,
                resumed: false,
            },
        );
        f.apply(&stream, 2, AgentEventKind::TurnStarted { turn_id: turn });
        for seq in 3..16 {
            f.apply(&stream, seq, AgentEventKind::TextDelta);
        }
        f.apply(&stream, 16, AgentEventKind::SessionEnded { outcome });
        f.phase("sixteenth-event-terminal", &[], &[], &[(id, text)]);
        f.phase(
            "next-poll-worker-completion",
            &[completion(id, outcome)],
            &[],
            &[],
        );
    }
    let issue = AgentRuntimeIssue {
        task_id: ids[3],
        detail: format!(
            "Synthetic preparation unavailable; token sk-{}; check fixture configuration",
            "a".repeat(32)
        ),
    };
    f.phase(
        "report-only-issue",
        &[],
        &[issue],
        &[(ids[3], "Native Agent runtime issue; details in Tasks")],
    );
    assert!(
        f.state
            .issue_detail(ids[3])
            .unwrap()
            .contains("Synthetic preparation unavailable")
    );
    assert!(!f.label.text().contains(&format!("sk-{}", "a".repeat(32))));
    f.phase("final-no-replay", &[], &[], &[]);
    atomic_json(
        &f.dir.join("finished.json"),
        serde_json::json!({"phases":f.phase,"pid":std::process::id()}),
    );
    wait_file(&f.dir.join("finish.ack"));
    f.window.close();
}
