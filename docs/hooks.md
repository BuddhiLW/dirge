# Command hooks

dirge runs **Claude-Code-compatible command hooks**: shell commands registered
per lifecycle event, fed a JSON payload on stdin, answering through their exit
code and an optional JSON object on stdout. A hook written for Claude Code runs
unchanged.

Janet [plugins](plugins.md) stay the in-process extension point; command hooks
are the out-of-process one. Both can be active: plugin hooks run first, and a
plugin block short-circuits the command hooks for that call.

## Configuring

Two sources, concatenated per event:

1. `hooks` in dirge's `config.json` (global `~/.config/dirge/config.json`,
   then project `.dirge/config.json`). Same shape as Claude Code's `hooks` key:

   ```json
   {
     "hooks": {
       "PreToolUse": [
         { "matcher": "Bash",
           "hooks": [{ "type": "command", "command": "~/bin/check-bash.sh", "timeout": 10 }] }
       ]
     }
   }
   ```

2. With `"claude_hooks": true`, the `hooks` blocks of Claude Code's own
   settings files: `~/.claude/settings.json`, `<project>/.claude/settings.json`
   and `<project>/.claude/settings.local.json`. One switch reuses whatever is
   already enabled for Claude Code.

A command runs as `sh -c <command>` (so `~` expands) with `CLAUDE_PROJECT_DIR`,
`DIRGE_PROJECT_DIR` and `DIRGE_HOOK=1` set. `timeout` is in seconds (default 60).

### Addon entries

With the `addons` feature, an entry can name a handler an [addon](addons.md)
registered instead of a command. No process starts; the addon answers
in-process:

```json
{ "type": "addon", "addon": "hive.dirge", "handler": "guard", "timeout": 10 }
```

The addon registers the handler under `:dirge/command-hooks` in its `hooks`
map, keyed by name, as a fn of `{:payload <the event's JSON payload>}`:

```clojure
{:dirge/command-hooks {"guard" (fn [{:keys [payload]}] ...)}}
```

Its answer is read as the process it stands in for, then decoded exactly as a
command's output (see [Answering](#answering)):

- a map with an integer `exit` (and optional `stdout`, `stderr` strings) is that
  exit, verbatim;
- `nil` is exit 0 with nothing on stdout;
- a string is exit 0 with that string on stdout;
- anything else, typically a Claude-style JSON answer, is exit 0 with its JSON
  on stdout.

An addon entry fails open like a command: no addon host running, an addon or
handler that is not loaded, a handler that throws, or no answer within
`timeout` all allow the action and are logged. `SessionStart`,
`UserPromptSubmit` and `SubagentStart` run on dirge's event loop, where the
addon isolate answers within 5 seconds and refuses anything that waits on the
loop, such as an MCP call; a handler that needs one allows those moments
unjudged. The tool events and `Stop` run off the loop and can wait.

### Other entry types

`type` is open. An entry of any other type goes to the runner installed for
that type (`command_hooks::boundary::install_runner`), which reads its own
fields off the entry. With no runner for the type, such as Claude Code's
`prompt` entries, the entry is kept but decides nothing, and the skip is
logged at debug level.

## Events

| Event | Fires | Matcher is matched against | Effect |
|---|---|---|---|
| `PreToolUse` | before every tool call (main agent and subagents) | Claude tool name, and dirge's | block refuses the call; context rides on the result; `updatedInput` rewrites the args |
| `PostToolUse` | after every tool call | Claude tool name, and dirge's | block reason and context are appended to the result |
| `SessionStart` | first run of a session (`source`: `startup` / `resume`) | `source` | context is appended to the system prompt |
| `UserPromptSubmit` | each user prompt | (all groups) | context is prepended to the prompt; a block ends the run with the hook's reason and the model is not called |
| `SubagentStart` | a `task` subagent is forked (`agent_type`: `task`) | `agent_type` | context is appended to the child's system prompt |
| `Stop` / `SubagentStop` | the main agent / a subagent is about to finish | (all groups) | a block feeds its reason back and the agent continues (`stop_hook_active` is set on the next check; at most 8 in a row) |
| `PreCompact` | a compaction is about to run: `/compact` (`trigger`: `manual`), or a fold when the context fills up (`trigger`: `auto`) | `trigger` | observe only: the compaction runs whatever the hook answers; a block is logged and ignored |

Matchers follow Claude Code: absent, `""` or `"*"` match everything;
otherwise the pattern is a regex anchored to the whole name (`Edit|Write`,
`mcp__.*`).

### Events dirge does not fire itself

The event set is open. Entries under any other name, such as `Notification`,
run whenever a seam fires that name (`HookEvent::named("Notification")`),
through the same pipeline and decoder. Such an event also reaches every
addon that registered the hook key `:dirge.hook/<Event>`, with no entry
needed. The addon's hook gets the event's payload, and its answer is read
like an addon entry's:

```clojure
{:dirge.hook/Notification
 (fn [{:keys [message]}]
   {:hookSpecificOutput {:additionalContext (str "heard " message)}})}
```

The events in the table above reach addons through their own hook points
(`:dirge/before-tool-call` and the rest), not through this key. The one
exception is `PreCompact`: dirge fires it as an open event, so an addon
hears it through `:dirge.hook/PreCompact` with no entry configured.

`PreCompact` fires once per compaction, before the conversation is
summarized: from `/compact`, from the pre-send compaction when a prompt
would push the context past 85% of the window, from the recovery after a
context-overflow error, and from the agent loop's own folds. A `/compact`
that finds nothing old enough to compact does not fire it. A hook that answers
slowly delays the compaction by that long (up to its `timeout`).

## Payload

Every payload carries `hook_event_name`, `session_id`, `cwd`,
`transcript_path` (`null`) and `harness: "dirge"`. Tool events add
`tool_name`, `tool_input`, `tool_use_id` (and `tool_response` for
`PostToolUse`). `PreCompact` adds `trigger` (`manual` / `auto`) and
`custom_instructions` (the `/compact` argument; empty otherwise).

Tool calls are restated in Claude's vocabulary, so hooks written for Claude
Code match:

| dirge | `tool_name` | `tool_input` additions |
|---|---|---|
| `bash`, `bash_output`, `kill_shell` | `Bash`, `BashOutput`, `KillShell` | none |
| `read`, `read_minified` | `Read` | `file_path` (absolute) |
| `write` | `Write` | `file_path` |
| `edit`, `edit_lines`, `edit_minified` | `Edit` | `file_path`, `old_string`, `new_string` |
| `apply_patch` | one `Write`/`Edit` per operation, judged separately (first block wins) | `file_path`, `old_string`, `new_string`, `content` |
| `grep` | `Grep` | none |
| `glob`, `find_files` | `Glob` | none |
| `list_dir`, `task`, `webfetch`, `websearch`, `write_todo_list`, `skill` | `LS`, `Task`, `WebFetch`, `WebSearch`, `TodoWrite`, `Skill` | none |
| MCP tools | unchanged (`mcp__server__tool`) | none |

dirge's own keys (`path`, `old_text`, ...) are kept alongside.

## Answering

- **exit 0**, stdout empty or JSON:
  - `hookSpecificOutput.permissionDecision: "deny"` blocks, with
    `permissionDecisionReason`;
  - `"ask"` defers to dirge's normal permission flow (its reason becomes context);
  - `hookSpecificOutput.additionalContext` is shown to the model;
  - `hookSpecificOutput.updatedInput` rewrites a `PreToolUse` call's input;
  - top-level `decision: "block"` + `reason` blocks (`Stop`, `PostToolUse`, ...).
  - For `SessionStart`, `UserPromptSubmit` and `SubagentStart`, plain non-JSON
    stdout is context.
- **exit 2** blocks, with stderr as the reason.
- **any other exit, a timeout, or a command that does not start** allows the
  action and is logged (`dirge::hooks` target). Hooks fail open.

`permissionDecision: "allow"` does not bypass dirge's permission checks.

## Reusing Claude Code hooks

If your hooks are already registered in `~/.claude/settings.json`, enabling
them for dirge is one line in `~/.config/dirge/config.json`:

```json
{ "claude_hooks": true }
```
