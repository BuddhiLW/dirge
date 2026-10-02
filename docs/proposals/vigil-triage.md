# Vigil Triage — a gate that only costs you when it's worth it

## Status

Draft. Ships in slice 06 (`vigil/06-emit-issue-lev`, PR #861, still draft) as a
refactor of the binary fail-open rite gate into a typed, cost-sensitive deferral
seam. The learning loop that recalibrates the gate lands later; the seam it
depends on ships here.

## What this is

Point a vigil at something that emits events and it watches cheaply: a heartbeat,
a file change, a socket message. When something matters, it wakes an agent that
can act. You get told only when it's worth telling you. The one concrete job we
are targeting first is a CI/flaky-test watcher — it pings you when a failure
pattern is new and stays quiet when it's the same flake you've already seen. The
same seam later covers disk/service health, feed/inbox triage, and on-call noise
filtering, but slice 06 does not pretend to ship all of them.

The transferable idea underneath is narrow: a cheap gate decides when to spend an
expensive expert, under an explicit cost ratio, and the outcome eventually feeds
back to recalibrate the gate. Everything else is packaging.

## The one piece of theory that matters

This is an instance of metareasoning and learning-to-defer, not a literal
"System 1 gates System 2." The folk two-systems story — fast vs. slow, where fast
is intuitive and slow is deliberate — is not where the literature lives; modern
dual-process work is a family of theories that distinguish autonomous processes
from deliberative, working-memory-dependent ones, and most of cognitive science
now treats "two systems" as a simplification that often misleads. What survives
the translation is the separation of *signal* from *policy*: the oracle (lev, or
any detector) emits a calibrated confidence `P` that the event needs attention;
dirge owns the policy that turns `P` and a cost matrix into a decision. That
decision is optimal when the policy wakes iff `P > τ`, where `τ = C_fp / (C_fn +
C_fp)` — `C_fp` the cost of a false wake, `C_fn` the cost of a miss. The magic
number `0.8` was never arbitrary; it was this ratio hiding inside an env var,
asserting that a spurious wake is four times as costly as a missed intervention.
The refactor makes that value judgment explicit and per-vigil. Fail-open and
fail-closed are the two endpoints of the same dial, not a bug and its fix.

## What ships in slice 06

Three concrete changes, each traceable to the current source.

### 1. A typed verdict seam

The drainer's binary `Option<String>` (`Some(reason)` = blocked, `None` = passed)
becomes a typed verdict with full dispatch for three shapes:

- `Shroud { reason }` — skip the observance (today's block path);
- `Rouse` — wake the agent and run the observance (today's pass path);
- `Toil { commands }` — the gate already knows what to do, so run shell commands
  directly and skip the agent turn.

`Toil` reuses the existing commands-mode execution path (`sh -c`, reaper.rs),
and a new `harness/toil` Janet primitive lets a gate emit it. The wider deferral
vocabulary (`Dwell`, `Knell`, `Entreat`, `Rest`) is deliberately left out of the
enum in this slice; they are decision-theoretic variants that only make sense
against outcome data, so they land with the calibration loop rather than as
unused arms.

### 2. The cost matrix, explicit

Vigil config gains a `gate` section; the threshold is derived, not stored:

```json
{
  "name": "ci-watch",
  "trigger": { "type": "toll", "interval_secs": 60 },
  "gate": {
    "false_positive_cost": 4.0,
    "false_negative_cost": 1.0,
    "fail": "open"
  },
  "prompt": "..."
}
```

`false_positive_cost` / `false_negative_cost` are `C_fp` / `C_fn`; the runtime
computes `τ` and injects it into the rite hook context. `fail` is `open` (assume
`P = 1` on oracle outage), `closed` (assume `P = 0`), or `prior` (assume the
empirical base rate; currently falls back to open until outcome data exists).
Defaults preserve today's behavior: `C_fp = 4, C_fn = 1` (so `τ = 0.8`), `fail =
open`. The threshold leaves the plugin — lev's contract narrows to emitting the
signal.

### 3. Verdict persistence

Every verdict is written to `vigil_db` (a new `vigil_verdicts` table): vigil
name, trigger, verdict, reason, commands, threshold, timestamp. The block reason
stops being a `warn!`-and-discard and becomes the audit trail and the substrate
for the later calibration loop.

## What is deliberately deferred

- **The outcome loop is the headline, not an afterthought.** The reason this
  pattern is worth anything is that the watcher gets better at *your* environment
  from what actually mattered. Slice 08 adds an `on-vigil-outcome` hook,
  `(signal, outcome)` persistence, and a calibration surface ("when it said 80%,
  it was right 76% of the time"). That is what makes a stranger trust an
  autonomous watcher enough to leave it running.
- **Safe action is a public-trust problem.** `Toil` — the gate executing shell —
  is where this lives or dies as a product. Dry-run by default, an approval step
  for anything not on an allowlist, an undo log, and a hard off switch are all
  required before this is anything but an internal tool. None of it ships in
  slice 06.
- **Presets and named detectors as the onboarding path.** Two floats are the
  correct internal representation, and the derivation stays in the code comments.
  The public interface is named presets ("notify me only when it's sure") and
  built-in signal generators (regex, embedding similarity, z-score, a small
  local model) so no one has to write a plugin to get started. The plugin is the
  escape hatch, not the on-ramp.

## Rust / Janet boundary

lev stays out of the Rust core. dirge gains a generic, typed deferral contract —
the `GateVerdict` seam — with no lev-specific knowledge. lev is the first oracle
plugged into it, interchangeable with a pure-Janet heuristic or any other
sidecar. Build the seam that lets the policy learn, and lev's sophistication
compounds; wire lev in directly, and the whole line is hostage to one external
project.

## Testing

- Rust: unit tests for the drainer's verdict interpretation, the reaper's
  three-way dispatch (including a `Toil` command actually running), the
  derived-threshold cost math, and verdict persistence.
- Janet: `harness/toil` round-trips for both the JSON-array and bare-command
  forms.
- Integration: the compose smoke test should assert on the verdict in the log,
  not on dirge's exit code (a `Shroud` makes `--vigil-once` time out at 180s and
  exit 1 today — see the separate compose review).

## Open questions

1. **Cost defaults.** `C_fp = 4, C_fn = 1` preserves today's `τ = 0.8`. Whether
   a spurious wake is really four times worse than a miss is the one number worth
   a deliberate call, and it should eventually come from per-archetype priors
   rather than a global constant.
2. **Default fail posture.** `open` is backward-compatible; `closed` is the
   stronger stance for a gate that can act. The compose fixture can set `closed`
   while the code default stays `open`.

## Risks

- **Typing the oneshot touches the drainer contract**, shared by interactive and
  headless modes. Mechanical, but the fire-and-forget hooks (`respond_to: None`)
  must stay untouched.
- **`Toil` broadens the shell-execution surface.** It reuses the existing
  commands-mode path with its shell-quote hardening, but gate-emitted commands
  still run whatever a plugin (or a trusted vigil config) says.
- **Calibration is slow.** Outcome data accumulates at vigil cadence; the prior
  will be unstable early. Slice 08 must not promise accuracy it cannot yet
  measure.
