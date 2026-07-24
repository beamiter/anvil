# Welcome to jterm1 notebooks

A `.jtnb.md` file is **markdown** with runnable shell code fences. Unlabelled
and `shell` fences use jterm1's configured shell; explicit `bash`, `sh`, `zsh`,
`fish`, `pwsh`, and `powershell` fences use the named interpreter. Each
runnable cell has *Run*, *Stop*, and *Copy* controls.

Use *Run All* to execute runnable cells in order. *Stop All* terminates the
current run and clears the remaining queue.

Every run starts in the notebook's directory in its own process group. It does
**not** touch your active terminal, but it is not sandboxed. *Stop*, *Stop All*,
and closing the viewer terminate the interpreter and its descendants.

## Try it

```bash
echo "hello from a notebook cell"
```

## Multiple commands per cell

```bash
date
uname -srm
echo "cwd is $(pwd)"
```

## Non-zero exit codes

The exit status is shown after the cell finishes. A non-zero status is
highlighted.

```bash
ls /this/path/does/not/exist
```

## Long-running cells

Use *Stop* to cancel this cell's process group, or *Stop All* to cancel it and
clear any queued cells.

```bash
echo "starting"; sleep 30; echo "done"
```

## Other languages

Only unlabelled, `shell`, `bash`, `sh`, `zsh`, `fish`, `pwsh`, and
`powershell` fences are runnable. Other languages render as read-only snippets.

```python
print("This is a python snippet — display only.")
```

```rust
fn main() {
    println!("Rust snippet — also display only.");
}
```

## Markdown caveats

Inline formatting is minimal: `# / ## / ###` for headings, `**bold**`,
`*italic*`, and `` `inline code` ``. Tables, nested lists and images are
*not* rendered — they'll appear as literal markdown text. The goal is
runnable cells, not a full markdown reader.
