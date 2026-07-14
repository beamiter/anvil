from pathlib import Path
import re

MAIN = Path("src/main.rs")
text = MAIN.read_text()

for anchor, declaration in [
    ("mod agent;\n", "mod agent_ops;\n"),
    ("mod file_tree;\n", "mod file_tree_ops;\n"),
    ("mod workflows;\n", "mod workflow_ops;\n"),
]:
    if declaration not in text:
        text = text.replace(anchor, anchor + declaration, 1)


def expose(block: str) -> str:
    return re.sub(r"^    fn ", "    pub(crate) fn ", block, flags=re.MULTILINE)


def extract(text: str, start_marker: str, end_marker: str):
    start = text.find(start_marker)
    end = text.find(end_marker, start)
    if start < 0 or end < 0:
        raise SystemExit(f"extraction markers not found: {start_marker!r} -> {end_marker!r}")
    return text[:start] + text[end:], text[start:end]

text, workflow_block = extract(
    text,
    "    /// Re-scan the user's workflow directory.",
    "    /// `?` palette accept handler:",
)

text, agent_block = extract(
    text,
    "    // ── Agent mode",
    "    /// Open the session-level AI panel",
)

text, file_tree_block = extract(
    text,
    "    /// Rebuild the file tree with `root` at the top.",
    "}\n\n#[allow(deprecated)]\nfn install_static_css",
)

workflow_module = '''//! Workflow discovery, lookup, rendering, and dialog dispatch.
//!
//! These remain inherent operations on the existing Relm4 `AppModel`; this
//! module only separates workflow responsibilities from the component lifecycle.

use super::*;

impl AppModel {
''' + expose(workflow_block) + "}\n"

agent_module = '''//! AI Agent session orchestration for the Relm4 application model.
//!
//! The existing `AppModel`, `AppMsg`, Relm4 controllers, and update loop remain
//! authoritative. This module only groups the Agent-specific inherent methods.

use super::*;

impl AppModel {
''' + expose(agent_block) + "}\n"

file_tree_module = '''//! File-tree root and navigation operations.
//!
//! These GTK operations remain methods of the same Relm4 `AppModel` and keep the
//! existing file-tree store, header controller, and message routing unchanged.

use super::*;

impl AppModel {
''' + expose(file_tree_block) + "}\n"

MAIN.write_text(text)
Path("src/workflow_ops.rs").write_text(workflow_module)
Path("src/agent_ops.rs").write_text(agent_module)
Path("src/file_tree_ops.rs").write_text(file_tree_module)
