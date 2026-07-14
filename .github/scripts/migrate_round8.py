from pathlib import Path
import re

MAIN = Path("src/main.rs")
text = MAIN.read_text()

if "mod config_ops;\n" not in text:
    text = text.replace("mod config;\n", "mod config;\nmod config_ops;\n", 1)
if "mod navigation_ui;\n" not in text:
    text = text.replace("mod notebook;\n", "mod notebook;\nmod navigation_ui;\n", 1)

config_start = text.find("    fn reload_config(")
nav_start = text.find("    /// Move the tab strip into the holder", config_start)
impl_end_marker = "}\n\n#[allow(deprecated)]\nfn install_static_css"
impl_end = text.find(impl_end_marker, nav_start)
if min(config_start, nav_start, impl_end) < 0:
    raise SystemExit("config/navigation extraction markers not found")

config_block = text[config_start:nav_start]
navigation_block = text[nav_start:impl_end]

# Methods move into sibling modules, so expose the same AppModel API crate-wide.
def expose(block: str) -> str:
    return re.sub(r"^    fn ", "    pub(crate) fn ", block, flags=re.MULTILINE)

config_module = '''//! Configuration reload and dynamic appearance operations.
//!
//! These are inherent methods on the existing Relm4 `AppModel`. The module
//! separates configuration responsibilities without introducing another model,
//! controller framework, or event loop.

use super::*;

impl AppModel {
''' + expose(config_block) + "}\n"

navigation_module = '''//! Tab-strip placement, sidebar view, and navigation presentation operations.
//!
//! This remains part of the same Relm4 `AppModel`; it only moves GTK presentation
//! helpers out of the component lifecycle implementation.

use super::*;

impl AppModel {
''' + expose(navigation_block) + "}\n"

Path("src/config_ops.rs").write_text(config_module)
Path("src/navigation_ui.rs").write_text(navigation_module)
MAIN.write_text(text[:config_start] + text[impl_end:])
