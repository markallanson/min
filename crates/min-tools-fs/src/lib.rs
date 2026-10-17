//! min-tools-fs — file tools: `read`, `edit`, `write`.
//!
//! `edit` is the workhorse for self-modification: exact-match replacement
//! where each `old_text` must match exactly one place. That constraint keeps
//! the model honest (it must read first) and makes edits atomic.

use min_core::{Hub, Plugin, Tool};
use serde_json::Value;

pub struct FsPlugin;

impl Plugin for FsPlugin {
    fn name(&self) -> &'static str {
        "fs"
    }
    fn start(&mut self, hub: &Hub) {
        let mut tools = hub.tools();
        tools.register(Box::new(ReadTool));
        tools.register(Box::new(EditTool));
        tools.register(Box::new(WriteTool));
    }
}

struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn spec(&self) -> &'static str {
        r#"{"description":"Read a text file. Lines are numbered like `cat -n`. Use offset/limit for large files; without limit, the first 2000 lines are shown. Returns an error if the file does not exist.","input_schema":{"type":"object","properties":{"path":{"type":"string","description":"file path (relative to cwd or absolute)"},"offset":{"type":"integer","description":"1-based line number to start from"},"limit":{"type":"integer","description":"maximum number of lines to return"}},"required":["path"]}}"#
    }
    fn run(&self, input: &str) -> Result<String, String> {
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let path = v["path"].as_str().ok_or("missing `path`")?;
        let offset = v["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = v["limit"].as_u64().unwrap_or(2000).clamp(1, 5000) as usize;
        let raw = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
        let text = String::from_utf8_lossy(&raw);
        let lines: Vec<&str> = text.lines().collect();
        if offset > lines.len() {
            return Ok(format!("(offset {offset} is past end of file; file has {} lines)", lines.len()));
        }
        let end = (offset - 1 + limit).min(lines.len());
        let mut out = String::new();
        for (i, line) in lines[offset - 1..end].iter().enumerate() {
            out.push_str(&format!("{:>5}| {}\n", offset + i, line));
        }
        if end < lines.len() {
            out.push_str(&format!(
                "({} more lines below — call again with offset={})\n",
                lines.len() - end,
                end + 1
            ));
        }
        Ok(out)
    }
}

struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn spec(&self) -> &'static str {
        r#"{"description":"Edit a file with a list of exact string replacements. Each old_text must occur exactly once in the current content (edits apply in order, so later edits see earlier results). Include enough surrounding context to make old_text unique. All edits are validated before anything is written: if one fails, nothing changes. Use `read` first.","input_schema":{"type":"object","properties":{"path":{"type":"string"},"edits":{"type":"array","items":{"type":"object","properties":{"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["old_text","new_text"]}}},"required":["path","edits"]}}"#
    }
    fn run(&self, input: &str) -> Result<String, String> {
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let path = v["path"].as_str().ok_or("missing `path`")?;
        let edits = v["edits"].as_array().ok_or("missing `edits` array")?;
        if edits.is_empty() {
            return Err("no edits given".into());
        }
        let mut content = std::fs::read_to_string(path)
            .map_err(|e| format!("read {path}: {e} (only UTF-8 files can be edited — use write)"))?;
        for e in edits {
            let old = e["old_text"].as_str().ok_or("edit missing `old_text`")?;
            let new = e["new_text"].as_str().ok_or("edit missing `new_text`")?;
            if old == new {
                return Err(format!("{path}: an old_text is identical to its new_text"));
            }
            match content.match_indices(old).count() {
                0 => {
                    return Err(format!(
                        "{path}: old_text not found — re-read the file and copy the text exactly"
                    ))
                }
                1 => content = content.replacen(old, new, 1),
                n => {
                    return Err(format!(
                        "{path}: old_text matches {n} places — include more context to make it unique"
                    ))
                }
            }
        }
        std::fs::write(path, &content).map_err(|e| format!("write {path}: {e}"))?;
        Ok(format!("{path}: applied {} edit(s)", edits.len()))
    }
}

struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn spec(&self) -> &'static str {
        r#"{"description":"Create or overwrite a file with the given content. Parent directories are created. This replaces the whole file — for targeted changes prefer `edit`.","input_schema":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}"#
    }
    fn run(&self, input: &str) -> Result<String, String> {
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let path = v["path"].as_str().ok_or("missing `path`")?;
        let content = v["content"].as_str().ok_or("missing `content`")?;
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
        }
        std::fs::write(path, content).map_err(|e| format!("write {path}: {e}"))?;
        Ok(format!("{path}: wrote {} bytes", content.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("min-fs-test-{name}-{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    fn tool_input(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn edit_applies_unique_replacement() {
        let path = tmp("edit-ok");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let out = EditTool
            .run(&tool_input(json!({"path": path, "edits": [{"old_text": "beta", "new_text": "BETA"}]})))
            .unwrap();
        assert!(out.contains("applied 1 edit"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nBETA\ngamma\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn edit_rejects_ambiguous_and_missing() {
        let path = tmp("edit-amb");
        std::fs::write(&path, "x x x\n").unwrap();
        let err = EditTool
            .run(&tool_input(json!({"path": path, "edits": [{"old_text": "x", "new_text": "y"}]})))
            .unwrap_err();
        assert!(err.contains("3 places"), "{err}");
        let err = EditTool
            .run(&tool_input(json!({"path": path, "edits": [{"old_text": "nope", "new_text": "y"}]})))
            .unwrap_err();
        assert!(err.contains("not found"), "{err}");
        // failed edits must not modify the file
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x x x\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_numbers_lines_and_notes_truncation() {
        let path = tmp("read-lines");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let out = ReadTool.run(&tool_input(json!({"path": path, "limit": 2}))).unwrap();
        assert!(out.contains("    1| one"));
        assert!(out.contains("    2| two"));
        assert!(out.contains("more lines below"));
        std::fs::remove_file(&path).ok();
    }
}
