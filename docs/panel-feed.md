# Panel feed

dirge can subscribe to an external **panel feed**: a local HTTP
producer that streams Server-Sent Events. Each event opens, updates or
closes a panel in the left side panel, or posts a one-line
notification. dirge can answer the producer through a small reply
endpoint.

The feed is **off by default**. Nothing is contacted unless a
`panel_feed` block is configured.

## Configuration

In `~/.config/dirge/config.json` (or the project `.dirge/config.json`):

```json
{ "panel_feed": { "discovery_dir": "my-producer" } }
```

or, with a fixed endpoint:

```json
{ "panel_feed": { "url": "http://127.0.0.1:7777/feed", "token_file": "/run/user/1000/feed.token" } }
```

| key             | meaning                                                                                   |
|-----------------|-------------------------------------------------------------------------------------------|
| `enabled`       | `false` switches the feed off while keeping the rest of the block. Default: on when a source is set. |
| `discovery_dir` | Directory holding a discovery file named `dirge.json`. A relative path is taken under `$XDG_RUNTIME_DIR` (else the system temp dir). |
| `url`           | Explicit base URL. Takes precedence over `discovery_dir`.                                 |
| `token_file`    | File holding the token for `url` (surrounding whitespace is trimmed).                     |
| `loop`          | `false` ignores `loop/*` ops (the feed stays display-only). Default: on. See [Loop ops](#loop-ops). |

The source is re-read on every (re)connect, so a producer that restarts
on a new port with a new token is picked up without restarting dirge.

### Discovery file

`<discovery_dir>/dirge.json`:

```json
{ "url": "http://127.0.0.1:43127/prefix", "token": "0123abcd...", "port": 43127, "pid": 4242 }
```

Only `url` (must be `http://` or `https://`; a trailing `/` is dropped),
`token` (optional) and `capabilities` (optional, below) are read; other
keys are ignored.

#### Capabilities

A producer says which replies it accepts and which grid keys send them.
dirge keeps no producer verbs of its own; without `capabilities` it
assumes the defaults below.

```json
"capabilities": {
  "version": 1,
  "replies": ["focus", "unfocus", "next-tab", "prev-tab", "refresh", "invoke"],
  "invokes": ["open", "run"],
  "keys": {"enter": "focus", "u": "unfocus", "r": "refresh",
           "tab": "next-tab", "shift-tab": "prev-tab", "o": {"invoke": "open"}}
}
```

- `version`: only `1` is read; any other keeps every default.
- `replies`: the verbs `/panel <verb>` and the grid may send. Absent: the
  defaults `focus <id>`, `unfocus`, `next-tab`, `prev-tab`, `refresh`. A
  default verb keeps its arity (`focus` needs an id); any other takes an
  optional id. `invoke` names the invoke reply, not a verb.
- `keys`: grid keys, in keymap chord syntax (`enter`, `tab`,
  `shift-tab`, `u`), to a reply verb or to `{"invoke": verb}` on the
  selected panel cell. A key whose verb is not in `replies` (or, when
  `invokes` is given, not in `invokes`) is dropped, as is a chord with
  Ctrl or Alt. Absent: Tab, Shift+Tab, r, u and Enter bound to the
  default verbs that `replies` allows.
- dirge's own grid keys (Esc/q, arrows and hjkl, Home/End, 1-9, and
  Enter/m on a subagent cell) cannot be rebound. Enter on a panel cell
  is the producer's.

A reply verb that names an item gets the selected panel cell's id.

Every file that carries a token (the discovery file or `token_file`)
must be a regular file, not a symlink, **owned by the current user**
and with **mode 0600** (no group or other bits). Anything else is
refused and retried later. The token is never logged; error messages
and debug output redact it.

## Transport

With base URL `<url>` and token `<token>` (percent-encoded):

| request                              | purpose                                     |
|--------------------------------------|---------------------------------------------|
| `GET  <url>/events?token=<token>`    | the event stream (`Accept: text/event-stream`) |
| `POST <url>/reply?token=<token>`     | one JSON reply (`Content-Type: application/json`) |

When there is no token, the `token` parameter is omitted. dirge never
sends an `Origin` header and never routes feed requests through a
proxy; producers should refuse any request that carries an `Origin`
and any request whose token does not match (e.g. `401`).

### Stream

Standard Server-Sent Events. dirge ignores the `event:` type: every
`data:` payload is one JSON object naming its own `op`. `id:` is
accepted and ignored, `retry: <ms>` sets the reconnect base delay
(clamped to 100 ms .. 60 s), and comment lines (`: ping`) serve as
heartbeats.

A stream silent for 60 s (no event, no comment) counts as dead.
Producers should send a heartbeat comment more often than that, e.g.
every 15 s.

Example frame:

```
id: 12
event: panel
data: {"op":"notify","message":"build finished","level":"info"}

```

### Reconnection

On any failure (producer not running, refused connection, non-2xx
answer, stream end, parse error, idle timeout) dirge waits and
reconnects, forever, until it exits. The delay is exponential backoff
from the base delay (1 s, or the last `retry:`) capped at 30 s, with
jitter: each wait is uniformly between half and all of the current
step. A connection that delivered at least one event resets the
backoff.

When a connection ends, every panel it showed is closed, so no stale
state lingers. Producers should therefore replay the current state of
every live panel (their latest `show`) to each new
connection.

## Ops

Unknown ops, non-object payloads and ops missing a required field are
ignored; the stream continues.

Only the names below are read. hive-vessel's older `:json` names
(`ui/show-panel`, `panel/id`, ...) are unknown ops; hive-vessel sends the
neutral names to any client that asks for `features`, which dirge always
does.

### `show`

Create panel `id`, or replace its title and body.

```json
{ "op": "show",
  "id": "builds",
  "title": "Builds",
  "lines": [ {"text": "Builds", "face": "title"},
             {"text": "", "face": "plain"},
             {"text": "api   passing", "face": "success"},
             "a bare string is a plain line" ] }
```

- `id`: required, non-blank.
- `title`: optional. Otherwise `doc["title"]` when a `doc` object is
  present, else the id.
- `lines`: array of strings or `{text, face}` objects. A `text`
  containing newlines becomes several rows with the same face. If
  `lines` is absent, a string `text` field is used as the body.
- A line may instead be `{face, spans: [...]}`: styled runs painted
  left to right on one row, each a string (in the line's face) or a
  `{text, face}` object. A newline inside a run starts a new row and
  keeps the run's face. Spans are honoured when the cljrs view engine
  owns the panels (see below); the plain fallback paints the `text`.

When dirge runs the cljrs view engine (`--features addons`, the
default engine there), the panels' policy is not in core: every op
reaches the `dirge.panels` reducer undecoded, and it answers with
`paint` / `unpaint` effects that core only sanitises, bounds and
draws. When the stream ends the reducer hears `{"op": "feed/ended"}`
and closes what the producer opened. Without that engine core applies
the ops itself, as described here.
- If the first line equals the title with face `title`/`heading`, it
  (and one following blank line) is dropped, so the title is not
  painted twice.
- Any other fields (such as a structured `doc`) are ignored.

### `close`

```json
{ "op": "close", "id": "builds" }
```

### `focus`

Create or retitle an accumulating (log-style) panel and focus it.

```json
{ "op": "focus", "id": "log", "title": "Activity" }
```

### `append`

Append one line to a log-style panel, creating it when absent.

```json
{ "op": "append", "id": "log", "line": {"text": "step 3 done", "face": "muted"} }
```

`line` may also be a bare string, or the line can be given as top-level
`text` and `face`.

### `notify`

```json
{ "op": "notify", "message": "deploy finished", "level": "warn" }
```

`message` (or `text`) is required; `level` is `info` (default), `warn`
or `error`. Terminal escape sequences are stripped.

### Faces

| face                                         | painted as |
|----------------------------------------------|------------|
| `title`, `heading`, `link`, `hunk`, `info`, `accent` | accent |
| `success`, `added`, `ok`                     | success    |
| `warn`, `warning`                            | warning    |
| `error`, `removed`                           | error      |
| `muted`, `dim`, `comment`                    | dimmed     |
| `plain`, `code`, anything else, absent       | normal     |

All producer text is sanitised (control and escape sequences removed)
and bounded in length and count by the panel layer.

## Loop ops

Every op above only changes what the human sees. A `loop/*` op changes
what the agent does: it is a directive for the running agent loop. dirge
advertises it by subscribing with `features=loop` (unless the config says
`"loop": false`), so a producer can hold these ops back from clients that
would drop them.

```json
{"op": "loop/steer", "id": "s-17", "prompt": "[hive sense · ling-1 is blocked]\nneed the schema\n\nTo unblock it, ..."}
```

| op | effect |
|----|--------|
| `loop/steer` | Injected before the next model call of the running turn, between tool rounds, with a preamble saying it is an external event and not the user. With no run active it opens a turn. |
| `loop/interject` | The running turn ends at its next boundary (the same graceful stop as the user's interjection) and the message opens the next turn. |
| `loop/followup` | Delivered when the running turn is about to finish, so the run continues with it. With no run active it opens a turn. |

`id` and `prompt` are required (`text` is accepted in place of `prompt`);
the prompt is exactly what the model reads. Other fields are ignored.
Each op also shows one line in the chat, so the human sees what changed
the agent's course.

An op reaches the loop once. When it does, dirge replies
`{"action": "ack", "target": "<id>"}`. A producer should keep an op until
it is acknowledged and send it again on the next connection; dirge
replaces a queued op that has the same `id` instead of running it twice.
Only the main session's runs take loop ops. Subagents never see them.

## Replies

dirge POSTs one JSON object per request to `<url>/reply`. Any 2xx
(typically `204 No Content`) is success.

```json
{"action": "focus", "target": "<item id>"}
{"action": "unfocus"}
{"action": "next-tab"}
{"action": "prev-tab"}
{"action": "refresh"}
{"action": "ack", "target": "<loop op id>"}
```

- `focus`: focus the item `target` (an id the producer showed).
- `unfocus`: leave the focused view.
- `next-tab` / `prev-tab`: move between the producer's views.
- `refresh`: ask the producer to repaint everything it shows.
- `ack`: the loop op `target` reached the agent loop (sent once per op,
  automatically; see [Loop ops](#loop-ops)).

Producers should accept and ignore actions they do not know.

From the TUI, `/panel next`, `/panel prev`, `/panel refresh`,
`/panel unfocus` and `/panel focus <id>` send these replies, and the
global keys Alt+. (next), Alt+, (previous) and Alt+/ (refresh) do the
same without leaving the prompt (rebindable as `panel_next_tab`,
`panel_prev_tab` and `panel_refresh`). A global key sends its verb only
when the producer accepts it (its `replies`, else the defaults);
otherwise dirge shows a warning and sends nothing. A reply that fails (no feed
running, the producer unreachable or answering non-2xx) is shown as a
notification in the chat area.

Keys a `show` op declares (`keys`) fire only while the panel holds key
focus: press Alt+P (`focus_panel`) to give it focus, Esc or Alt+P to
return to the prompt. Until then every typed letter goes to the prompt.

## Swarm grid

The left side panel shows external panels as compact boxes. `/swarm`
(or Alt+S, rebindable as `toggle_swarm`) opens the swarm grid instead:
every external panel painted at full size, one cell per panel, over
the chat and both side panels, followed by one cell per in-flight
subagent. The input strip and status line stay below it. `/swarm on`
and `/swarm off` open or close it explicitly.

A subagent cell is titled with its agent profile (or `subagent`) and
its short id. Its body is the `[AGENTS]` preview line (`↳ elapsed · N tools ·
<last tool call>`) followed by the newest lines of the
subagent's chat tab, so the grid shows what every subagent is doing
without switching tabs. The cell disappears when the subagent
finishes; its chat tab keeps the transcript.

The grid repaints the latest state the producer sent; it keeps no
history of earlier frames. A panel the producer focused (`FocusTab`) is
painted first and marked `●`; an accumulating panel (`AppendTab`) shows
its newest lines. Each cell's title carries its number (`2/5`), and the
selected cell is marked `▸` and drawn in the accent colour. When not all
cells fit, the grid shows the page that holds the selected cell.

While the grid is open, keys drive it rather than the prompt:

| Key | Action |
|-----|--------|
| Tab / Shift+Tab | reply `next-tab` / `prev-tab` |
| r | reply `refresh` |
| Enter | panel: reply `focus` with the selected panel's id as `target`; subagent: close the grid and switch to its chat tab |
| m | subagent: close the grid and start `/msg <id> ` in the editor |
| u | reply `unfocus` |
| Arrows, h/j/k/l, 1-9, Home/End | move the selection (local, no reply) |
| Esc, q, Alt+S | close the grid |

These are the same replies `/panel` sends, over the same channel. Other
global keys (scrolling, Ctrl+L, Alt+. / Alt+, / Alt+/) keep working,
and Ctrl+C still interrupts a running agent. A permission prompt or
question from the agent takes the keys while it is shown.

## Lens capabilities (wire v2)

On every subscription dirge sends `GET /events?token=...&features=spans,keys,cursor,open-file`
(the comma list is URL encoded). `features` is the capability list for this
version of the panel feed, not a version of the producer; an absent parameter
means the v1 plain-lines client. Producers must degrade to plain lines and
legacy replies for clients without a capability. Replies do not carry this
parameter.

With `keys`, `show` may include `"keys":{"n":"next","Enter":"open"}`.
Only keys declared by the focused panel are claimed; the producer decides
what verbs mean. With `cursor`, `"cursor":true` enables j/k, Up/Down and
PgUp/PgDn navigation over body rows. A line may include an `"id"` (stable
row identifier); moving the cursor highlights the row and scrolls the panel
without sending a reply. Declared keys send the selected row id (or null):

```json
{"action":"invoke","panel":"carto-flow","verb":"next","row":null,"payload":{}}
```

The five existing focus/unfocus/next-tab/prev-tab/refresh replies remain
valid aliases. `open-file` accepts `path`, optional 1-based `line`, and
optional unified `diff`. The path must resolve to an existing regular file
inside the current project root, including after symlink resolution. dirge
opens it in the configured external editor, or previews its content in a
panel; if `diff` is present it previews the supplied diff instead. A refused
path produces a notice, never an editor launch.
