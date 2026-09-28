//! dirge tool calls restated in Claude Code's tool vocabulary.
//!
//! Hooks written for Claude Code key on its tool names (`Bash`, `Read`,
//! `Edit`, ...) and argument keys (`file_path`, `old_string`, ...). Each
//! dirge call is restated as one or more Claude calls. Keys are only
//! added, never removed, so a hook that knows dirge's own names still
//! finds them.

use std::path::Path;

use serde_json::{Map, Value, json};

/// Claude Code's name for a dirge tool. Unknown names (MCP tools are
/// already `mcp__server__tool`) pass through.
pub fn claude_tool_name(name: &str) -> String {
    match name {
        "bash" => "Bash",
        "bash_output" => "BashOutput",
        "kill_shell" => "KillShell",
        "read" | "read_minified" => "Read",
        "write" => "Write",
        "edit" | "edit_lines" | "edit_minified" | "apply_patch" => "Edit",
        "grep" => "Grep",
        "glob" | "find_files" => "Glob",
        "list_dir" => "LS",
        "task" => "Task",
        "webfetch" => "WebFetch",
        "websearch" => "WebSearch",
        "write_todo_list" => "TodoWrite",
        "skill" => "Skill",
        other => other,
    }
    .to_string()
}

fn is_file_tool(name: &str) -> bool {
    matches!(
        name,
        "read" | "read_minified" | "write" | "edit" | "edit_lines" | "edit_minified"
    )
}

/// One dirge call as the Claude calls a hook judges. `apply_patch`
/// yields one call per operation; everything else yields exactly one.
pub fn claude_calls(name: &str, args: &Value, cwd: &Path) -> Vec<(String, Value)> {
    if name == "apply_patch" {
        let ops = args
            .get("operations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let calls: Vec<(String, Value)> =
            ops.iter().flat_map(|op| patch_op_calls(op, cwd)).collect();
        if !calls.is_empty() {
            return calls;
        }
    }
    vec![(claude_tool_name(name), claude_input(name, args, cwd))]
}

/// `args` with Claude's keys added alongside dirge's.
pub fn claude_input(name: &str, args: &Value, cwd: &Path) -> Value {
    let Value::Object(obj) = args else {
        return args.clone();
    };
    let mut out = obj.clone();
    if is_file_tool(name)
        && let Some(path) = obj.get("path").and_then(Value::as_str)
    {
        out.entry("file_path")
            .or_insert_with(|| json!(absolute(path, cwd)));
    }
    alias(&mut out, "old_text", "old_string");
    alias(&mut out, "new_text", "new_string");
    Value::Object(out)
}

/// A hook's `updatedInput` (Claude dialect) folded back onto the original
/// dirge args. `apply_patch` is left untouched: one rewrite cannot be
/// mapped back onto several operations.
pub fn dirge_args_from_claude(name: &str, original: &Value, updated: &Value) -> Value {
    let (Value::Object(orig), Value::Object(upd)) = (original, updated) else {
        return original.clone();
    };
    if name == "apply_patch" {
        return original.clone();
    }
    let renames: &[(&str, &str)] = if is_file_tool(name) {
        &[
            ("file_path", "path"),
            ("old_string", "old_text"),
            ("new_string", "new_text"),
        ]
    } else {
        &[("old_string", "old_text"), ("new_string", "new_text")]
    };
    let mut out = orig.clone();
    for (k, v) in upd {
        if !renames.iter().any(|(claude, _)| claude == k) {
            out.insert(k.clone(), v.clone());
        }
    }
    for (claude, dirge) in renames {
        if let Some(v) = upd.get(*claude) {
            out.insert((*dirge).to_string(), v.clone());
        }
    }
    Value::Object(out)
}

fn patch_op_calls(op: &Value, cwd: &Path) -> Vec<(String, Value)> {
    let path = op.get("path").and_then(Value::as_str).unwrap_or("");
    let file_path = absolute(path, cwd);
    let text = |k: &str| op.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match op.get("action").and_then(Value::as_str) {
        Some("create") => vec![(
            "Write".into(),
            json!({ "file_path": file_path, "content": text("content") }),
        )],
        Some("update") => vec![(
            "Edit".into(),
            json!({ "file_path": file_path, "old_string": text("old_text"), "new_string": text("new_text") }),
        )],
        Some("delete") => vec![(
            "Write".into(),
            json!({ "file_path": file_path, "content": "" }),
        )],
        Some("rename") => {
            let new_path = absolute(&text("new_path"), cwd);
            vec![
                (
                    "Edit".into(),
                    json!({ "file_path": file_path, "old_string": "", "new_string": "" }),
                ),
                (
                    "Write".into(),
                    json!({ "file_path": new_path, "content": "" }),
                ),
            ]
        }
        _ => Vec::new(),
    }
}

fn alias(obj: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(v) = obj.get(from).cloned() {
        obj.entry(to).or_insert(v);
    }
}

fn absolute(path: &str, cwd: &Path) -> String {
    let p = Path::new(path);
    if p.is_absolute() || path.is_empty() {
        path.to_string()
    } else {
        cwd.join(p).display().to_string()
    }
}

/// Text blocks of a tool result joined, for `tool_response`.
pub fn result_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}
