# Vigil Triage, Extended — The Economic and Metareasoning Layer

## Status

Draft proposal. This extends `vigil-triage.md` with the theory that doc
deliberately deferred: the cost-matrix derivation, the classic-ML machinery
that should sit behind it, and the self-improving loop that turns the gate
from a configured threshold into a system that tunes its own parameters and
priors from observed outcomes. It changes nothing about what ships in slice 06
(the typed `GateVerdict` seam, the explicit cost matrix, verdict persistence).
It defines what the calibration loop (slice 08 and beyond) should compute, and
it answers one question directly: Black-Scholes is not the tool.

## Why not Black-Scholes

Black-Scholes prices an option: a right, but not the obligation, to transact
one underlying asset at a strike price on a maturity date, under the assumption
that the asset follows geometric Brownian motion and can be hedged by
continuously trading the underlying. It needs four things a vigil gate does not
have:

- an **underlying asset** whose price we observe and can trade against;
- a **strike and maturity** (a discrete contract, not a recurring decision);
- a **risk-free rate** and a **hedging portfolio** that make the price
  arbitrage-free;
- a **continuous-time diffusion** model of the underlying.

A gate is a repeated discrete decision under an asymmetric cost matrix. There is
no asset, no strike, no hedging, no arbitrage argument. Invoking Black-Scholes
here would be cargo-culted finance: the symbols would appear, but the
assumptions that make them mean anything are absent. The correct frame is
Bayes-optimal choice under a cost matrix — decision theory, not option pricing.

## The economic frame: a cost matrix, not a threshold

Let `P = P(Y=1)` be the probability that the event warrants an agent
intervention. That is what lev's `noul` is trying to estimate; `P` is the
thing that must eventually be calibrated to a true probability.

The two actions the seam supports today are `Shroud` (block, do nothing) and
`Rouse` (wake the agent). Their costs:

| action   | Y=1 (needs attention) | Y=0 (does not) |
| -------- | --------------------- | -------------- |
| Shroud   | `C_fn`                | 0              |
| Rouse    | 0                     | `C_fp`         |

`C_fn` is the cost of a missed intervention (a blocked event that needed
waking). `C_fp` is the cost of a spurious wake (a turn spent on nothing). The
expected costs are

```
E[Shroud] = P * C_fn
E[Rouse]  = (1 - P) * C_fp
```

Wake when `E[Rouse] < E[Shroud]`:

```
(1 - P) * C_fp < P * C_fn
P > C_fp / (C_fn + C_fp) = tau
```

That is the whole derivation. The hardcoded `LEV_THRESHOLD=0.8` was never a
magic number; it was this ratio in disguise: `C_fp=4, C_fn=1` gives
`tau = 4/5 = 0.8`, i.e. "a spurious wake is priced at four times a missed
intervention." `vigil-triage.md` states this invariant; the point of this doc
is that `tau` is not a constant to configure but a parameter to estimate.

Two notes that keep the two-parameter form honest:

- The table above normalizes the correct-wake cost to zero. If you insist on a
  separate per-turn cost `C_wake` (the LLM call is never free even when
  justified), it folds cleanly into the false-positive term: the same threshold
  holds with `C_fp' = C_fp + C_wake`. Two costs are already the general
  two-action form.
- As `C_fp` grows relative to `C_fn`, `tau -> 1` (very conservative; wake only
  near-certain events). As `C_fn` dominates, `tau -> 0` (wake on any hint).
  The single scalar `tau` is the summary of that tradeoff, not an independent
  input.

## Generalizing to three actions: the Toil region

`Toil` adds a third action: run cheap shell commands directly, skip the agent
turn. Model it as an action that reliably handles the event at a fixed cost
`C_toil < C_fp` (it is cheaper than a full wake). The cost matrix becomes

| action   | Y=1    | Y=0     |
| -------- | ------ | ------- |
| Shroud   | `C_fn` | 0       |
| Toil     | `C_toil` | `C_toil` |
| Rouse    | 0      | `C_fp`  |

Comparing expected costs pairwise gives two thresholds, not one:

```
E[Shroud] = P * C_fn
E[Toil]   = C_toil
E[Rouse]  = (1 - P) * C_fp

Shroud vs Toil: P * C_fn < C_toil        -> P < C_toil / C_fn          = tau_low
Toil   vs Rouse: C_toil < (1 - P) * C_fp -> P < 1 - C_toil / C_fp      = tau_high
```

Three regions:

- `P < tau_low` -> `Shroud` (the event is not worth even cheap work);
- `tau_low < P < tau_high` -> `Toil` (cheap deterministic action wins);
- `P > tau_high` -> `Rouse` (spend the deliberative turn).

The Toil region is non-empty only when `tau_low < tau_high`, which requires

```
C_toil < C_fn * C_fp / (C_fn + C_fp)
```

With the default matrix (`C_fn=1, C_fp=4`) that is `C_toil < 0.8`: a Toil action
must cost less than four-fifths of a miss to earn its own region. For a
concrete example, `C_toil = 0.5` (half a miss) gives `tau_low = 0.5`,
`tau_high = 0.875` — Shroud below 0.5, Toil between 0.5 and 0.875, Rouse above
0.875. This is what gives Shroud/Rouse/Toil a genuine economic ordering rather
than ad-hoc rules, and it is the answer to "what formula orders the verdicts":
the thresholds are ratios of the cost matrix, nothing else.

The `q=1` assumption (Toil always resolves the event) is the simplification to
relax first; see open questions.

## The classic ML that actually fits

Three pieces, each answering a different failure mode of a naive gate. None of
this ships in slice 06; it is the target architecture for the calibration loop.

### 1. Calibration — make `noul` a true probability

`tau` only means anything if `P` is a probability, not an arbitrary score.
Calibration is the property `E[Y | noul = p] = p`: when lev says 0.8, the event
really warrants waking 80% of the time. This is the metacognitive substrate —
the system's estimate of its own uncertainty, made honest against outcomes.

The mechanics are standard and small:

- **Platt scaling** fits a logistic map from raw `noul` to calibrated `P`, or
- **isotonic regression** fits a monotone non-parametric map when the
  score-outcome relationship is not logistic.

Either fits on the `(signal, outcome)` pairs persisted from the outcome loop.
The deliverable is a reliability curve ("when it said 80%, it was right 76% of
the time") plus per-bin confidence intervals from a beta posterior — the thing
that makes a stranger trust an autonomous watcher.

### 2. Online cost estimation — Thompson sampling

The costs `C_fp`, `C_fn`, `C_toil` are not known constants; they are unknown
quantities we learn. Each verdict is an arm pull (`Shroud`/`Rouse`/`Toil`); the
observed outcome (resolved / missed / wasted) is the reward. Maintain a
posterior over the cost parameters, and sample from that posterior to compute
`tau` for the next decision. That is Thompson sampling, and it is exactly the
"tune parameters and priors" mechanism: `tau` moves with the evidence, and the
global `4:1` default becomes a prior to be updated, not a value to be asserted.

The same machinery updates lev's prior (its base rate), which feeds the `fail:
prior` posture. Parameters and priors self-tune; no code changes are needed for
that part of the loop.

### 3. Optimal stopping — metareasoning as a stopping problem

The third question a gate should ask is not "what do I do" but "do I have
enough evidence to decide, or should I pay for one more cheap signal first?"
Model the hidden state `Y` with sequential observations and a stopping action:
continue gathering (cheap) signal, or commit to a verdict (expensive on
average). This is a sequential probability ratio test (SPRT) in the two-action
case, or a POMDP in general. The value of one more observation is the expected
reduction in decision cost it buys.

This is where "gather more evidence before committing" lives, and it is the
formal layer that earns the name metareasoning: the system reasons about when
to stop reasoning.

## The control architecture: a utility-augmented behavior tree

The economics answer *what the right decision is*; they do not answer *how that
decision is structured, authored, and audited.* Behavior trees (BTs) already
solved the second problem — first in game AI, and now directly for
language-model agents (Kelley 2024). Four points in that lineage matter, in
order:

**LLM agents first — Dendron (Kelley 2024, arXiv:2404.07439).** The closest prior
art to this slice: BTs as structured programming for language-model agents,
combining the model with classical AI and deterministic code. Of its case
studies, two are directly on point. The infrastructure-inspection agent shows a
BT coordinating perception, model calls, and action without the model steering
the whole pipeline; the safety-constraint agent shows a BT enforcing a
constraint the model was never reliably taught — the model proposes, the tree
disposes. That second pattern is the argument for this whole design: the gate's
policy lives in structure and arithmetic the tree can enforce, not in the
model's judgment alone.

**Origin — children compete, parents decide (Isla, GDC 2005).** Isla's system is
a behavior DAG in which non-leaf nodes decide which child to run, via either
parent-authored code or child competition on a "desire-to-run" (relevancy)
score. Two of his techniques carry over nearly verbatim. The *impulse*: a
free-floating trigger injected into the tree at a defined position, because
"tree-placement constitutes as large a part of the decision process as does its
relevancy function" — the impulse only fires after higher-priority behaviors
have had their say. And *behavior tagging*: common conditions are hoisted into
tags that lock and unlock whole branches per state, so one tree serves several
roles, each with different unlocked regions.

**The fix for static priority — the utility selector (Mark & Dill, *Game AI Pro*
2011; the Infinite Axis Utility System).** The acknowledged biggest drawback of a
vanilla BT is fixed priority order: a plain selector checks its children in a
baked-in sequence, so one option always wins the tie regardless of context. The
utility selector instead queries every child for a utility score and picks
dynamically, propagating utility up through composites and letting decorators
transform it on the way. It also separates *evaluation* (score all candidates)
from *execution* (run the winner) — a split that matters for the audit story
below.

**Learning and verification (Colledanchise & Ögren, *Behavior Trees in Robotics
and AI*; Colledanchise, Parasuraman & Ögren 2019).** BTs are modular, reactive,
and human-readable, and — the part that matters for a gate that acts — they have
formal state-space tools for analyzing safety, robustness, and liveness. The
learning literature is the cautionary tail: genetic programming can synthesize
tree *structure*, but the result needs anti-bloat control and is not something
one auto-deploys.

Mapped onto the triage, the pieces line up with the economics almost one to one:

- **The gate is a degenerate BT: a single utility selector.** `Shroud`, `Rouse`,
  and `Toil` are the children; the cost matrix *is* the utility function (pick
  minimum expected cost, i.e. maximum utility); `tau_low` and `tau_high` are the
  crossover points where two utility curves trade places. The "formula that
  orders the verdicts" is a utility selector with the cost matrix as its score.
  Slice 06's typed seam is exactly this flat selector, and it should say so
  plainly: a tree of one composite and three leaves.

- **Static priority was the bug.** `LEV_THRESHOLD=0.8` was a hardcoded priority
  in a static tree. The utility-selector lesson is that priority must be a
  function of the event, not a constant — which is precisely why `tau` is
  derived, not stored. Game AI reached this the same way, after hand-tuned
  priority orderings stopped scaling.

- **`Running` is `Dwell`.** The status that separates a BT from a decision tree
  or FSM is `Running`: a node may report "not done yet" and be re-ticked. That
  is the structural home for the deferral verdict and for the optimal-stopping
  layer above — a stopping decorator returns `Running` while evidence sits below
  the stopping bound and commits (`Success`/`Failure`) once it crosses. "Should
  I gather one more signal" becomes a node in the tree, not a sidecar subsystem.

- **Impulses are escalation.** A watcher/harbinger escalation is an impulse
  injected at a defined tree position that preempts a `Running` (Dwell) subtree,
  after higher-priority branches have declined. Tree-placement semantics replace
  ad-hoc preemption logic.

- **Tags are archetypes.** Per-archetype gates (CI-watch vs file-watch vs
  security-watch) are one tree with different tag-unlocked branches and
  different cost priors — not separate programs with duplicated policy.

- **Evaluation vs execution is the audit trail and the bandit's ledger.** The
  tick trace — which nodes ran, their utility scores, which returned
  `Success`/`Failure`/`Running` — is a stronger record in `vigil_db` than a
  single threshold, and it is exactly the bookkeeping surface the calibration
  and Thompson-sampling updates read and write.

- **Verification is the public-trust story.** Because a BT has formal
  safety/liveness analysis, hand-authoring the topology while bounding the
  learned parameters yields a gate whose behavior is analyzable rather than an
  opaque learned policy. The human-gated structural-regression step becomes
  "propose a subtree edit," and the GP literature's caution (learned structure
  is not auto-deployable) is the standing argument for keeping that step
  human-gated.

The honest scope note: none of the structure beyond the flat selector earns its
keep until `Dwell` (deferral) and escalation exist. Before then the BT framing
is a correctness check on the typed seam — it demands that verdicts stay typed
leaves with explicit costs so the selector can be expressed at all — not a
reason to build a tree runtime in slice 06.

## The self-improving loop

The pieces compose into a single data flow:

1. The gate emits a verdict and its calibrated `P` (the signal).
2. The outcome arrives later via `on-vigil-outcome`: `(signal, outcome)` —
   resolved, missed, or wasted.
3. The bandit updates the cost and calibration posteriors.
4. `tau` and lev's prior shift; the gate re-tunes without code changes.
5. **Structural regression** — when the posterior says the parameterization
   itself is wrong (calibration is systematically off, or an unmodeled feature
   dominates the residual), the system proposes a change. That change is a
   human-gated code review, never an auto-merge.
6. **Public good** — the calibrated reliability curves and the cost posteriors
   are aggregates with no event content. Those are shareable, so a fleet can
   learn from pooled outcomes without any one user shipping their event data.

The public-good claim deserves the same scrutiny the rest of this doc applies
to itself: what is pooled, under what policy, and how a stranger verifies the
aggregate without seeing the events is itself an open design problem, not a
footnote. It is listed below rather than waved away.

## Slice map

- **Slice 06 (this PR):** typed `GateVerdict` seam, explicit cost matrix, verdict
  persistence. No learning, no outcome loop. `tau` is derived from configured
  costs, not estimated.
- **Slice 08:** `on-vigil-outcome` hook, `(signal, outcome)` persistence, the
  calibration surface (reliability curve + bin posteriors), empirical priors.
  Batch calibration only — no online updates yet.
- **Slice 09:** online Thompson-sampling updates for `tau`, the cost posteriors,
  and lev's prior; enable the Toil region once Toil outcomes exist.
- **Slice 10+:** optimal stopping for multi-observation events, and the
  public-good sharing protocol as its own reviewed design.

## Open questions

1. **Toil reliability.** The `q=1` assumption (Toil always resolves the event)
   is false in general. A reliability parameter `q < 1` shifts `tau_low` and
   `tau_high`; it should enter the model before Toil is anything but dry-run.
2. **Per-turn cost.** `C_wake` is currently folded into `C_fp`. Is that
   acceptable, or does the public interface need it as a third explicit term?
3. **Cost priors per archetype.** The `4:1` default is a global constant. The
   bandit priors should be per-archetype (CI-watch vs file-watch vs
   security-watch have different economics); that needs an archetype taxonomy
   first.
4. **Calibration method.** Platt vs isotonic depends on how monotone the
   score-outcome curve is; the choice should come from the data, not be assumed.
5. **Public sharing.** What exactly is pooled, under what privacy and policy
   envelope, and how the aggregate is audited without exposing events.
6. **When does the tree stop being a flat selector?** The BT runtime earns its
   keep only once `Dwell` (Running) and escalation (impulses) exist. Whether to
   introduce it in slice 10 or defer until the deferral vocabulary is proven is
   a genuine scoping call, not a foregone conclusion.

## Risks

- **Early instability.** Posteriors are wide until outcomes accumulate at vigil
  cadence. A self-tuning `tau` will wander early; the loop must not present
  unstable numbers as calibrated confidence.
- **Outcome mislabeling.** The outcome itself is a judgment (did the event
  "really" need attention?). Garbage outcomes train the bandit toward garbage
  costs; the labeling step needs its own scrutiny before it becomes training
  data.
- **Slow sample rate.** Vigil cadence is the data rate. Calibration and bandit
  updates will be slow; that is a reason for pooled (public) learning, and also
  a reason to keep the batch surface separate from the online one.
- **Auto-tuned `tau` is a policy surface.** A threshold that moves on its own is
  a change in behavior a user did not make. Every `tau` change must be visible
  and reversible, with the cost posterior that justified it on the record.
- **BT machinery before deferral exists.** A tree runtime with only one
  composite and three leaves is ceremony, not architecture. The risk is adopting
  the structure (ticks, node statuses, decorators) for its own sake before
  `Dwell` and escalation give `Running` and impulses something to do.

## Sources

- Kelley, R. "Behavior Trees Enable Structured Programming of Language Model
  Agents." arXiv:2404.07439, 2024 (the Dendron library).
- Isla, D. "Handling Complexity in the Halo 2 AI." Game Developers Conference,
  2005.
- Mark, D., and K. Dill. "Building Utility Decisions into Your Existing
  Behavior Tree." *Game AI Pro*, 2011. (See also Mark, D., *Behavioral
  Mathematics for Game AI*, 2009, and the Infinite Axis Utility System
  documentation.)
- Colledanchise, M., and P. Ögren. *Behavior Trees in Robotics and AI: An
  Introduction*. CRC Press, 2018 (arXiv:1709.00084).
- Colledanchise, M., R. Parasuraman, and P. Ögren. "Learning of Behavior Trees
  for Autonomous Agents." *IEEE Transactions on Games*, 11(2), 2019.
