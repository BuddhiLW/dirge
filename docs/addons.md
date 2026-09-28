# Clojure addons

dirge can host addons written in Clojure. They run on an embedded
[clojurust](https://github.com/BuddhiLW/clojurust) interpreter, beside Janet
plugins, and implement an `IAddon` protocol defined by a small Clojure
library that dirge loads by namespace. Written as portable `.cljc`, the same
addon also runs in any JVM host of that protocol.

Build with the feature enabled:

```bash
cargo build --release --features addons
```

## Installing an addon

An addon is a directory laid out like this:

```text
my-addon/
  deps.edn                              ; optional, see below
  src/my_addon/core.cljc
  resources/META-INF/addons/my-addon.edn
```

dirge searches for `META-INF/addons/*.edn` manifests under:

1. `<project>/.dirge/addons/`
2. `~/.config/dirge/addons/`
3. every directory listed in `addons.paths` in `config.json`

A symlink to an addon checkout works in any of them.

The addon's `src/` and `resources/` go on the interpreter's source path, plus
the `src/` of every `:local/root` dependency named in its `deps.edn`
(followed transitively). Anything else can be added with `addons.source_paths`
or the `DIRGE_ADDON_PATH` environment variable (a `:`-separated list). The
protocol library's source must be reachable one of these ways.

A manifest whose init namespace has no `.cljc` or `.cljrs` source on that path
is skipped, so JVM-only addons can share a repository with portable ones.

```json
{
  "addons": {
    "enabled": true,
    "paths": ["~/src/my-addons"],
    "source_paths": ["~/src/addon-protocol/src"],
    "protocol_ns": "my.addon-protocol"
  }
}
```

## The protocol

`addons.protocol_ns` names the namespace that defines the protocol. dirge
resolves these functions from it at startup and refuses to start the host if
a required one is missing:

| Function | Required | Meaning |
|---|---|---|
| `(addon? x)` | yes | whether `x` implements the protocol |
| `(initialize! addon config)` | yes | start; returns `{:success? bool :errors [...]}` |
| `(shutdown! addon)` | yes | release resources |
| `(tools addon)` | yes | tool definitions, see below |
| `(hooks addon)` | no | map of hook key to function, see below |
| `(health addon)` | no | `{:status :ok}` or similar |

## The manifest

```edn
{:addon/id      "my.addon"
 :addon/init-ns "my-addon.core"
 :addon/init-fn "addon-ctor"
 :addon/config  {}}
```

`init-fn` is called with `:addon/config` and must return an addon. dirge then
calls `initialize!` with `{:addon/id … :addon/config … :dirge/host {…}}`.

## Tools

Every entry of `(tools addon)` becomes a tool the model can call:

```clojure
{:name        "count-rows"
 :description "Counts the rows it is handed"
 :inputSchema {:type "object" :properties {:rows {:type "array"}}}
 :handler     (fn [params] {:content [{:type "text" :text (str (count (:rows params)))}]})}
```

Arguments arrive as a map with keyword keys. The handler may return the
result shape above (`:isError true` marks a failure), a string, or any data,
which the model sees as JSON. Names are restricted to `[A-Za-z0-9_-]`; a
built-in tool's name always wins, and between addons the first loaded wins.

Addon tools go through the same permission check as Janet plugin tools
(`plugin_tool`).

## Hooks

`(hooks addon)` returns a map from hook key to function. dirge calls these
keys and ignores the rest:

| Key | Called with | Return |
|---|---|---|
| `:dirge/system-prompt` | `{:cwd :session-id}` | text appended to the system prompt |
| `:dirge/on-prompt` | `{:prompt :session-id}` | text added before the user's prompt |
| `:dirge/before-tool-call` | `{:tool :args :tool-call-id}` | `nil`, `{:block "reason"}`, `{:context "text"}` or `{:args {…}}` |
| `:dirge/after-tool-call` | `{:tool :args :result :error?}` | text appended to the tool result |

Hooks run for the main session only, after Janet plugin and command hooks.
Subagents never run addon hooks. An exception in a hook is logged and
ignored.

`:dirge/system-prompt` and `:dirge/on-prompt` are skipped, with a log line,
while another call into the addons is still running (a command, a tool or a
tool-call hook, including a tool left running after its turn was
interrupted), and when they take longer than 5 seconds to answer.

## Slash commands

The `:dirge/commands` entry of the hooks map registers slash commands:

```clojure
{:dirge/commands
 {"rows" {:description "Count rows in the side panel"
          :handler     (fn [{:keys [args argv cwd]}] ...)}}}
```

`/rows a b` calls the handler with `{:args "a b" :argv ["a" "b"] :cwd …}`.
It returns `nil`, a string, or a map:

| Key | Effect |
|---|---|
| `:text` or `:markdown` | shown in the chat area |
| `:prompt` | submitted as the next prompt, starting a turn |

Command names are 1 to 32 characters of `[a-z0-9_:-]` starting with a letter.
Built-in commands and Janet plugin commands take precedence; between addons
the first loaded wins. Addon commands appear in `/help` and tab completion.

## Calling dirge

The `dirge.harness` namespace is available to addon code:

| Function | Effect |
|---|---|
| `(notify msg)` / `(notify msg level)` | a line in the chat area; `level` is `:info`, `:warn` or `:error` |
| `(log level msg)` | a log event on the `dirge::addon` target |
| `(cwd)` | dirge's working directory |
| `(version)` | the dirge version |
| `(tools)` | names of the dirge tools `call-tool` can run |
| `(call-tool name)` / `(call-tool name args)` | run a dirge tool (built-in or MCP) with the `args` map; answers `{:ok text}` or `{:error msg}` |
| `(panel op)` | a box in the side panel, see below; returns true when delivered |
| `(mcp-servers)` | names of the MCP servers dirge is connected to |
| `(mcp-call server tool)` / `(mcp-call server tool args)` | call an MCP tool over dirge's own connection |
| `(json-parse text)` | JSON text as data (keyword keys), or `nil` |

`panel` takes one of:

```clojure
{:op :show   :id "rows" :title "Rows" :lines ["plain" {:text "warned" :face "warn"}]}
{:op :show   :id "rows" :title "Rows" :markdown "# Rows\n- one"}
{:op :append :id "log"  :text "one more line" :face "dim"}
{:op :focus  :id "log"  :title "Log"}
{:op :close  :id "rows"}
```

Faces are `normal`, `dim`, `accent`, `success`, `warn` and `error`.

`mcp-call` answers the tool result (`{:content [...] :isError bool}`) or
`{:error "why"}`. It blocks until the server answers. It is refused (an
`{:error}` answer) while dirge's event loop is waiting on the addon, which is
the case for `:dirge/system-prompt`, `:dirge/on-prompt`, loading and
shutdown. Call it from commands, tools and tool-call hooks. Calls made this
way are not put through the per-tool permission prompt: an addon is code you
installed, running inside dirge.

`call-tool` goes through the tool's own permission check, so a call to
`bash` still asks the user. It refuses addon tools and `task`, and it is
unavailable from `:dirge/system-prompt` and `:dirge/on-prompt` hooks and
during load and shutdown, while dirge is waiting on the addon; call it from
a command, a tool or a tool-call hook.

To keep an addon portable, resolve these at call time, for example
`(when-let [f (resolve 'dirge.harness/notify)] (f "hi"))`, so the code is a
no-op on hosts without them.

## Lifecycle

Addons load at startup, before the first agent run, on a dedicated
interpreter thread. `shutdown!` runs for every addon when dirge exits. It
is skipped when a call into the addons is still running then, and dirge
waits at most 5 seconds for it to finish.

`/addons` lists the loaded addons with their tools, hooks, commands and
health, and any manifest that failed. `/addons reload` picks up changes
without restarting dirge:

1. every addon is shut down,
2. the loaded namespaces under each addon's own `src/` are evaluated again
   (the libraries it depends on, including the protocol, are not),
3. every manifest found now is loaded, constructed and initialized afresh.

New and removed tools reach the agent at the next prompt. It also starts the
host when dirge was started without any addon installed. Keep state that
must survive a reload in `defonce`. A reload does not pick up a changed
`protocol_ns`; restart for that.

## Example

`tests/fixtures/addons/` holds a minimal protocol namespace and an addon
that uses a tool, a hook, slash commands, `panel`, `mcp-call` and
`dirge.harness/notify`.
