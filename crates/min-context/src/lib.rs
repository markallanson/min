//! min-context — project context for the system prompt.
//!
//! First checks for a systemwide instruction file at
//! `~/.config/min/AGENTS.md` and, if present, emits it as a
//! [`events::SystemPromptSection`]. Then searches the directory tree under
//! the current directory for project instruction files (currently
//! `AGENTS.md`) and emits those the same way — root-most file first, so a
//! subdirectory's file arrives after (and can refine) its ancestors'. The
//! systemwide file is always the first context emitted by this plugin.
//! No files, no sections, nothing happens — the agent is unaffected.
//!
//! The round runs at startup and again whenever the conversation is
//! cleared: on [`events::ResetConversation`] this plugin re-walks the tree
//! and re-emits, preceded by [`events::ClearPromptSections`] so the agent
//! drops its previous copy and nothing stale survives.

use min_core::{events, Hub, Plugin};
use std::any::Any;

/// File names to look for in every directory of the tree.
const FILES: &[&str] = &["AGENTS.md"];

/// Directories never descended into, in addition to hidden ones (`.`-prefixed):
/// dependency trees and build output.
const SKIP_DIRS: &[&str] = &["node_modules", "target"];

/// Systemwide instruction file, relative to the user's home directory.
const SYSTEM_FILE: &str = ".config/min/AGENTS.md";

pub struct ContextPlugin;

impl ContextPlugin {
    /// One round: tell the agent to drop its sections, then hand over every
    /// instruction file found on disk right now.
    fn emit_sections(&self, hub: &Hub) {
        hub.emit(events::ClearPromptSections {});
        for (path, body) in system_file().into_iter().chain(collect(".")) {
            hub.emit(events::SystemPromptSection {
                title: format!("Project instructions ({path})"),
                body,
            });
        }
    }
}

impl Plugin for ContextPlugin {
    fn name(&self) -> &'static str {
        "context"
    }

    fn start(&mut self, hub: &Hub) {
        self.emit_sections(hub);
    }

    /// `/clear` doubles as a chance to re-read instructions that changed on
    /// disk. Context sits before the agent in load order, so the fresh
    /// sections arrive before the agent's own `ResetConversation` arm wipes
    /// the history — both happen within the one `/clear` dispatch.
    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if event.downcast_ref::<events::ResetConversation>().is_some() {
            self.emit_sections(hub);
        }
    }
}

/// The systemwide instruction file's (display path, contents), if it exists.
fn system_file() -> Option<(String, String)> {
    let home = std::env::home_dir()?;
    let path = home.join(SYSTEM_FILE);
    let body = std::fs::read_to_string(path).ok()?;
    Some((format!("~/{SYSTEM_FILE}"), body))
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
    use std::cell::RefCell;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;

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

    #[test]
    fn system_file_precedes_tree_when_home_points_at_scratch() {
        let root = scratch("system");
        fs::create_dir_all(root.join(".config/min")).unwrap();
        fs::write(root.join(".config/min/AGENTS.md"), "systemwide rules").unwrap();
        fs::write(root.join("AGENTS.md"), "project rules").unwrap();

        // HOME (tracked by home_dir on unix) aimed at the scratch dir; the
        // tree search covers the same dir but skips hidden entries, so the
        // systemwide file is found exactly once, first.
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &root);
        let found = system_file()
            .into_iter()
            .chain(collect(root.to_str().unwrap()))
            .collect::<Vec<_>>();
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            found,
            vec![
                ("~/.config/min/AGENTS.md".into(), "systemwide rules".into()),
                ("AGENTS.md".into(), "project rules".into()),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Records what context emits, in order, for the reset test below.
    struct Recorder(Rc<RefCell<Vec<String>>>);

    impl Plugin for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }

        fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
            if event.downcast_ref::<events::ClearPromptSections>().is_some() {
                self.0.borrow_mut().push("clear".into());
            } else if let Some(e) = event.downcast_ref::<events::SystemPromptSection>() {
                self.0.borrow_mut().push(format!("section:{}", e.body));
            }
        }
    }

    #[test]
    fn reset_conversation_clears_then_reemits_current_sections() {
        let root = scratch("reset");
        fs::create_dir_all(root.join(".config/min")).unwrap();
        fs::write(root.join(".config/min/AGENTS.md"), "fresh rules").unwrap();

        let log = Rc::new(RefCell::new(Vec::new()));
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &root);
        let hub = Hub::new(vec![
            Box::new(ContextPlugin),
            Box::new(Recorder(log.clone())),
        ]);
        // Emitted from outside any plugin, so every plugin sees it — exactly
        // what the core does when the repl returns it from `wait`.
        hub.emit(events::ResetConversation {});
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        // Clear first (nothing stale survives), then the file as it is now.
        // The tree walk under the scratch root finds no AGENTS.md, so the
        // systemwide file is the only section.
        assert_eq!(
            *log.borrow(),
            vec![
                "clear".to_string(),
                "section:fresh rules".to_string(),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }
}
