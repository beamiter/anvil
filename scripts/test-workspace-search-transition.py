#!/usr/bin/env python3
"""Run actual AppModel pane transitions against deterministic widget boundaries.

No GTK/PTY or model services are launched. Rust methods are extracted unchanged
from workspace_ops.rs; the geometry selector is loaded from the locked core
dependency. Only external controllers/tree operations are stubbed.
"""
from pathlib import Path
import json
import os
import re
import subprocess
import tempfile
import textwrap
ROOT = Path(__file__).resolve().parents[1]

def item(source, marker):
    assert source.count(marker) == 1, marker
    start = source.index(marker)
    line = source.rfind('\n', 0, start) + 1
    indent = source[line:start]
    opening = source.index('{', start)
    close = re.search(r'^' + re.escape(indent) + r'}[ \t]*$', source[opening:], re.M)
    assert close, marker
    return textwrap.dedent(source[line:opening + close.end()])

def main():
    source = Path(os.environ.get('WORKSPACE_SEARCH_SOURCE', ROOT / 'src/workspace_ops.rs')).read_text().split('#[cfg(test)]')[0]
    fixture = (ROOT / 'scripts/workspace_search_transition_harness.rs').read_text()
    metadata = json.loads(subprocess.run(
        [os.environ.get('CARGO', 'cargo'), 'metadata', '--locked', '--format-version=1'],
        cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE,
    ).stdout)
    core = next(package for package in metadata['packages'] if package['name'] == 'jterm_core')
    pane_layout = Path(core['manifest_path']).parent / 'src/pane_layout.rs'
    fixture = fixture.replace('// @pane_layout',
        '#[path = ' + json.dumps(str(pane_layout), ensure_ascii=False) + ']\nmod pane_layout;\n'
        'use pane_layout::{directional_focus_target, PaneDirection as Direction, PaneRect, Rect as PaneBounds};')
    methods = ['pub(crate) fn cycle_pane_focus(', 'pub(crate) fn focus_pane_directional(', 'pub(crate) fn close_pane(',
               'pub(crate) fn rollback_failed_split(', 'pub(crate) fn find_pane(',
               'pub(crate) fn close_tab(', 'pub(crate) fn close_tabs(', 'pub(crate) fn select_tab(']
    for marker in ['fn begin_search_pane_transition(', 'fn finish_search_pane_transition(', 'fn select_tab_from_pane(']:
        if marker in source:
            methods.append(marker)
    # Split creates a real terminal controller, unavailable in this fixture.
    # Guard the production commit closure's before/after ordering separately.
    split = item(source, 'pub(crate) fn split_active(')
    if 'begin_search_pane_transition' in split:
        assert split.index('begin_search_pane_transition') < split.index('slot.replace_with_split')
        assert split.index('tab.active_pane =') < split.index('finish_search_pane_transition')
        assert split.index('finish_search_pane_transition') < split.index('.emit(VteInput::GrabFocus)')
    fixture = fixture.replace('// @methods' , '\n'.join(item(source, marker) for marker in methods))
    fixture = fixture.replace('// @index', item(source, 'fn active_index_after_remove(') + '\n' + item(source, 'fn tab_selection_after_close('))
    fixture = fixture.replace('// @changed', item((ROOT / 'src/search.rs').read_text(), 'pub(crate) fn active_pane_changed('))
    with tempfile.TemporaryDirectory(prefix='workspace-search-') as d:
        rs = Path(d) / 'test.rs'; rs.write_text(fixture)
        binary = Path(d) / 'test'
        subprocess.run([os.environ.get('RUSTC', 'rustc'), '--edition=2021', '--test', str(rs), '-o', str(binary)], check=True)
        subprocess.run([str(binary), '--test-threads=1'], check=True)

if __name__ == '__main__':
    main()
