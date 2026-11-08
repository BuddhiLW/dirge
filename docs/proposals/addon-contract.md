# Proposal: one addon contract for every addon language (Janet onto AddonRuntime)

Status: draft for discussion with the dirge-code maintainers (raised by
allen-munsch on dirge-code#862 and #872; answered on #872, 2026-10-03). No code
until they say they want it.

## Problem

dirge has two plugin hosts that do not share a dispatch path:

| | Janet plugins | Clojure addons (`addons` feature) |
|---|---|---|
| Registry | `src/plugin/loader.rs` `HOOK_NAMES` (16 names, `on-init` .. `on-compact`) | `src/addons/domain.rs` `HookPoint` + open keys via `AddonRuntime::run_hook_key` |
| Runtime port | none: `src/plugin/worker.rs` is called directly | `src/addons/port.rs` `AddonRuntime` |
| Agent seam | plugin calls wired into the loop one by one | `src/agent/addon_hooks.rs` `AddonHooks`, installed once at the composition root |
| Shared | only the `plugin_tool` permission umbrella | |

Every new lifecycle moment has to be added twice, and the two lists already
differ (Janet has `transform-context` and `message-end`, addons have
`SessionEnd` and the open `dirge/*` keys). Two hosts with separate seams are
likely to end up as separate forks.

## Proposal

Make `AddonRuntime` the contract that any language host implements, the way an
LSP server implements the protocol and the editor does not care what language
it is written in.

1. **One contract.** `AddonRuntime` (load / unload / call_tool / run_command /
   run_hook / run_hook_key / post_hook) is the only interface. A host has to
   carry nothing else to be a dirge addon host.
2. **Janet becomes an adapter.** Add `JanetRuntime: AddonRuntime` over the
   existing `plugin::worker` thread. `load` wraps `plugin::loader::load_plugin`,
   `run_hook_key("on-tool-start", ctx)` calls the Janet function of that name,
   and replies map onto `HookReply`. The Janet plugin files and their API do
   not change.
3. **One registry.** `HOOK_NAMES` becomes the Janet adapter's alias table onto
   `HookPoint` / open keys (`on-tool-start` -> `BeforeToolCall`,
   `on-tool-end` -> `AfterToolCall`, `on-compact` -> `Compact`,
   `on-before-compact` -> the pre-fold point, `before-agent-start` ->
   `SystemPrompt`, and the rest -> open keys `dirge/<name>`). The loop fires
   each moment once, through `AddonHooks`.
4. **Composition.** A `MultiRuntime: AddonRuntime` fans each call out to the
   hosts that are present, in load order, and concatenates the replies. The
   default build installs only the Janet adapter. With `--features addons` the
   cljrs host is installed next to it. The default build stays lean because the
   Clojure host stays behind its feature.
5. **Permissions.** Both hosts keep going through the one `plugin_tool`
   permission check, which is now called from `AddonHooks::loop_tools` and no
   longer duplicated per host.

## Migration (each step ships on its own, behaviour unchanged)

1. Add `JanetRuntime` + `MultiRuntime` and their tests against a stub worker.
   Nothing calls them yet.
2. Route one moment (`on-tool-start` / `BeforeToolCall`) through
   `AddonHooks` for Janet, and delete the direct call. A recording-decorator
   test shows each Janet hook still fires exactly once.
3. Move the remaining moments over one PR at a time. Delete the direct calls
   and turn `HOOK_NAMES` into the alias table.
4. Document the contract in `docs/addons.md` as the thing a new language host
   implements.

## Open questions for the maintainers

- Is "one contract, many hosts" the direction you want, or should Janet stay
  the only built-in host and Clojure stay external?
- Is the Janet hook ordering relative to Clojure addons part of the API
  (proposal: Janet first, then addons, both in load order)?
- Do Janet plugins need the open-key hooks (`dirge/*`), or only the 16 names?
