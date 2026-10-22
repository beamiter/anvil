//! Compact, unambiguous native buffer labels. Kept identical across the four
//! frontends so each release can use its independently pinned core version.

use jterm_core::vim9::Vim9Buffer;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Shortest distinguishing path suffix, in snapshot order. This only reads
/// names: rendering never canonicalizes paths or performs filesystem I/O.
/// The full original name remains available to each frontend's tooltip.
pub(crate) fn vim9_buffer_labels(buffers: &[Vim9Buffer]) -> Vec<String> {
    let parts: Vec<Vec<&str>> = buffers
        .iter()
        .map(|buffer| {
            let parts: Vec<_> = Path::new(&buffer.name)
                .components()
                .map(|part| part.as_os_str().to_str().expect("UTF-8 buffer name"))
                .collect();
            if parts.is_empty() {
                vec!["[No Name]"]
            } else {
                parts
            }
        })
        .collect();
    // Sorting reversed components makes the nearest neighbours sufficient to
    // find each name's longest shared suffix. Only final labels allocate path
    // strings; deep paths do not generate every possible suffix each frame.
    let mut order: Vec<_> = (0..parts.len()).collect();
    order.sort_by(|&a, &b| parts[a].iter().rev().cmp(parts[b].iter().rev()));
    let mut depth = vec![1; buffers.len()];
    for pair in order.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let shared = parts[a]
            .iter()
            .rev()
            .zip(parts[b].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        depth[a] = depth[a].max((shared + 1).min(parts[a].len()));
        depth[b] = depth[b].max((shared + 1).min(parts[b].len()));
    }
    let labels: Vec<String> = parts
        .iter()
        .zip(&depth)
        .map(|(parts, &depth)| {
            parts[parts.len() - depth..]
                .iter()
                .collect::<PathBuf>()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let mut counts = HashMap::new();
    for label in &labels {
        *counts.entry(label.as_str()).or_insert(0) += 1;
    }
    // Multiple unnamed buffers or equivalent lexical paths still need an
    // identity. Reserve unique literal labels first so a filename that looks
    // like an ID suffix cannot collide with a generated label.
    let mut used: HashSet<String> = labels
        .iter()
        .filter(|label| counts[label.as_str()] == 1)
        .cloned()
        .collect();
    labels
        .iter()
        .zip(buffers)
        .map(|(label, buffer)| {
            if counts[label.as_str()] == 1 {
                return label.clone();
            }
            let suffix = format!(" [#{}]", buffer.id);
            let mut unique = format!("{label}{suffix}");
            while !used.insert(unique.clone()) {
                unique.push_str(&suffix);
            }
            unique
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(names: &[&str]) -> Vec<String> {
        let buffers: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, name)| Vim9Buffer {
                id: index as i64 + 1,
                name: (*name).into(),
                ..Default::default()
            })
            .collect();
        vim9_buffer_labels(&buffers)
    }

    #[test]
    fn distinct_basenames_stay_compact_and_snapshot_order_is_preserved() {
        assert_eq!(
            labels(&["/project/src/main.rs", "/project/Cargo.toml", ""]),
            ["main.rs", "Cargo.toml", "[No Name]"]
        );
        assert!(labels(&[]).is_empty());
    }

    #[test]
    fn same_names_expand_only_as_far_as_needed() {
        assert_eq!(
            labels(&[
                "/project/client/src/main.rs",
                "/project/server/src/main.rs",
                "/tmp/main.rs",
                "/project/lib.rs"
            ]),
            [
                "client/src/main.rs",
                "server/src/main.rs",
                "tmp/main.rs",
                "lib.rs"
            ]
        );
        assert_eq!(
            labels(&["main.rs", "/project/main.rs"]),
            ["main.rs", "project/main.rs"]
        );
        assert_eq!(labels(&["/main.rs", "main.rs"]), ["/main.rs", "main.rs"]);
    }

    #[test]
    fn unicode_and_missing_files_need_no_filesystem_access() {
        assert_eq!(
            labels(&[
                "/不存在/客户端/文档.md",
                "/不存在/服务端/文档.md",
                "term://shell"
            ]),
            ["客户端/文档.md", "服务端/文档.md", "shell"]
        );
    }

    #[test]
    fn unnamed_and_equivalent_names_have_unique_buffer_identities() {
        assert_eq!(
            labels(&["", "", "/project/./a.rs", "/project/a.rs", "[No Name] [#1]"]),
            [
                "[No Name] [#1] [#1]",
                "[No Name] [#2]",
                "/project/a.rs [#3]",
                "/project/a.rs [#4]",
                "[No Name] [#1]"
            ]
        );
    }
}
