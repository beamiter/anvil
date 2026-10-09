#!/usr/bin/env python3
"""Exercise the current Relm search component's exact update method with rustc.

Only widgets and the component sender are deterministic boundaries. The message
handling method, status/query helpers, and existing pure unit tests are read
from the checkout on every run. No GTK display or Cargo cache is required.
"""
from pathlib import Path
import hashlib
import os
import re
import subprocess
import tempfile
import textwrap

ROOT = Path(__file__).resolve().parents[1]


def item(source, marker):
    assert source.count(marker) == 1, marker
    start = source.index(marker)
    line_start = source.rfind("\n", 0, start) + 1
    indent = source[line_start:start]
    opening = source.index("{", start)
    closing = re.search(r"^" + re.escape(indent) + r"}[ \t]*$", source[opening:], re.M)
    assert closing
    return textwrap.dedent(source[line_start:opening + closing.end()])


def build_harness():
    source = (ROOT / "src/search.rs").read_text()
    types = source[source.index("/// Find-bar"):source.index("#[relm4::component")]
    update = item(source, "fn update_with_view(")
    helpers = source[source.index("fn sync_regex_mode("):]
    fixture = (ROOT / "scripts/search_lifecycle_harness.rs").read_text()
    for marker, value in [("types", types), ("update", update), ("helpers", helpers)]:
        placeholder = f"// @search-lifecycle:{marker}"
        assert fixture.count(placeholder) == 1
        fixture = fixture.replace(placeholder, value)
    return fixture


def main():
    with tempfile.TemporaryDirectory(prefix="anvil-search-lifecycle-") as directory:
        source = Path(directory) / "search_lifecycle.rs"
        binary = Path(directory) / "search_lifecycle"
        source.write_text(build_harness())
        subprocess.run([os.environ.get("RUSTC", "rustc"), "--edition=2021", "--test", "-C", "debuginfo=0", str(source), "-o", str(binary)], check=True)
        data = binary.read_bytes()
        print(f"Fixture ELF: {len(data)} bytes, SHA256 {hashlib.sha256(data).hexdigest()}", flush=True)
        subprocess.run([str(binary), "--test-threads=1"], check=True)


if __name__ == "__main__":
    main()
