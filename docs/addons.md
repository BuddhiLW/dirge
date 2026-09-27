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
An exception in a hook is logged and ignored.

## Calling dirge

The `dirge.harness` namespace is available to addon code:

| Function | Effect |
|---|---|
| `(notify msg)` / `(notify msg level)` | a line in the chat area; `level` is `:info`, `:warn` or `:error` |
| `(log level msg)` | a log event on the `dirge::addon` target |
| `(cwd)` | dirge's working directory |
| `(version)` | the dirge version |

To keep an addon portable, resolve these at call time, for example
`(when-let [f (resolve 'dirge.harness/notify)] (f "hi"))`, so the code is a
no-op on hosts without them.

## Lifecycle

Addons load at startup, before the first agent run, on a dedicated
interpreter thread. `shutdown!` runs for every addon when dirge exits.

## Example

`tests/fixtures/addons/` holds a minimal protocol namespace and an addon
that uses a tool, a hook and `dirge.harness/notify`.
