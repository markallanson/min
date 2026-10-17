//! min-context — project context for the system prompt.
//!
//! Searches the directory tree under the current directory for project
//! instruction files (currently `AGENTS.md`) at startup and emits them as
//! [`events::SystemPromptSection`] events for the agent to append to its
//! base system prompt — root-most file first, so a subdirectory's file
//! arrives after (and can refine) its ancestors'. No files, no sections,
//! nothing happens — the agent is unaffected.

use min_core::{events, Hub, Plugin};

/// File names to look for in every directory of the tree.
const FILES: &[&str] = &["AGENTS.md"];

/// Directories never descended into, in addition to hidden ones (`.`-prefixed):
/// dependency trees and build output.
const SKIP_DIRS: &[&str] = &["node_modules", "target"];

pub struct ContextPlugin;

impl Plugin for ContextPlugin {
    fn name(&self) -> &'static str {
        "context"
    }

    fn start(&mut self, hub: &Hub) {
        for (path, body) in collect(".") {
            hub.emit(events::SystemPromptSection {
                title: format!("Project instructions ({path})"),
                body,
            });
        }
    }
}

/// Every file named in [`FILES`] anywhere under `dir`, as (display path,
/// contents), ancestors before descendants. Hidden directories and
/// [`SKIP_DIRS`] are not searched; symlinks are never followed, so there
/// are no cycles.
fn collect(dir: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    walk(dir, "", &mut found);
    found
}

fn walk(dir: &str, prefix: &str, found: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());

    // This directory's own files first: an ancestor's instructions are
    // always collected before those of any of its subdirectories.
    for name in FILES {
        for entry in entries.iter().filter(|e| e.file_name() == *name) {
            if let Ok(body) = std::fs::read_to_string(entry.path()) {
                let path = if prefix.is_empty() {
                    (*name).to_string()
                } else {
                    format!("{prefix}/{name}")
                };
                found.push((path, body));
            }
        }
    }

    for entry in entries {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let sub = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        walk(&entry.path().to_string_lossy(), &sub, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("min-context-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_agents_md_in_subdirectories_ancestors_first() {
        let root = scratch("walk");
        fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        fs::create_dir_all(root.join("crates/min-agent")).unwrap();
        fs::write(root.join("crates/min-agent/AGENTS.md"), "agent rules").unwrap();
        // must not appear: hidden directory, dependency tree, unreadable body
        fs::create_dir_all(root.join("crates/zzz/.hidden")).unwrap();
        fs::write(root.join("crates/zzz/.hidden/AGENTS.md"), "secret").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/AGENTS.md"), "dep").unwrap();
        fs::create_dir_all(root.join("broken")).unwrap();
        fs::write(root.join("broken/AGENTS.md"), b"not valid utf8 \xff").unwrap();

        let found = collect(root.to_str().unwrap());
        assert_eq!(
            found,
            vec![
                ("AGENTS.md".into(), "root rules".into()),
                ("crates/min-agent/AGENTS.md".into(), "agent rules".into()),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_tree_yields_nothing() {
        let root = scratch("empty");
        assert!(collect(root.to_str().unwrap()).is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
