# Context-economy baseline: measuring the built-in fold

This document is the measurement protocol for any change to how dirge keeps a
long session inside its context budget: the fold ladder
(`src/agent/agent_loop/context_manager.rs`), the compaction pass
(`run_compaction_pass_with_focus`), the summary prompt
(`src/agent/compression.rs`), and anything that later replaces or augments
them (a different summarizer, eviction with recall handles, compression of tool
output on arrival).

It also records the **baseline**: what the built-in fold scores today. A
candidate is judged against these numbers, in the regime it claims to help,
and never on token counts alone.

Every result below says which rung of evidence it stands on:

- **measured**: produced by the commands in this document, raw numbers given;
- **reproduced**: re-run and matched;
- **inferred**: reasoned from code or from other numbers, not observed.

## Instruments

There are two, because they answer different questions.

### I1. Single-call recall (does a fold keep the facts?)

`src/agent/compaction_recall.rs` plants facts in the region a fold removes and
scores verbatim survival after the summarizer runs. Two layers:

- **Deterministic, offline** (`cargo test compaction_recall`): every planted
  fact must reach the prompt handed to the summarizer. This is the part dirge
  controls (window selection, serialization, per-turn truncation). No model,
  no network. A candidate that changes the window or the serializer must keep
  these green.
- **Live** (`src/agent/compaction_bakeoff.rs`, off unless `DIRGE_BAKEOFF=1`):
  one summarizer call per repeat against a real model.
  - *hard recall*: 20 unannounced facts buried in noisy tool output; score is
    facts kept out of 20, plus the tail (runs losing 2 or more).
  - *tool-call probe*: 6 facts that exist only in tool-call arguments; scored
    both in the prompt (serializer) and in the summary (model).
  - *coverage probe*: the prompt is clipped in half; does the summary say its
    source was partial?

The live probe holds the transcript, window, budget and scorer byte-identical
across arms, so a difference is attributable to the summarizer path alone. It
does not see agent behaviour.

### I2. Long-horizon pass rate and cost-per-pass (does the session still work?)

`scripts/loop-ab.sh -s fold-chain` runs dirge headless on a task that cannot
be finished without surviving several folds:

- a pointer chain of 24 small files, each naming the next under an unguessable
  name, so the reads are forced to be sequential (one turn per file);
- each file carries one hash-derived `VALUE`; the answer is their sum, so every
  value read before a fold must survive it;
- nothing may be written (a run that parks values on disk bypasses the thing
  under test and fails);
- `context_target=26000` pins the budget identically for every arm; each file
  stays under the aggressive per-result cap so no value is lost to result
  truncation, only to a fold.

The existing `compact` scenario is not sufficient: a model that reads its eight
files in one parallel turn answers on turn two and never folds (measured: 2
turns, 0 compactions on the model below).

`scripts/fold-report.py` reads the kept run directories and reports per arm.

## Metrics

| Metric | Source | Direction |
|---|---|---|
| pass rate, Wilson 95% CI | `check_correct`: exact sum, tree untouched | higher |
| folds per run | `context compacted` log lines | mechanism gate (see below) |
| input / cached / output tokens, mean with 95% CI | session file counters | lower (cached: higher) |
| cost per run (token-priced) | tokens x published price | lower |
| **measured spend** | provider balance before and after the batch | lower |
| **cost per pass** | measured spend / passes | lower; the headline cost metric |
| re-reads per run | `read` calls on a path already read | lower; the visible cost of a fold that lost something |
| turns | gates tally | context only |
| recall (I1) | facts kept / 20, runs losing 2+ | higher / lower |

**Mechanism gate.** A run with zero folds did not exercise the fold. Such runs
are counted and reported, never averaged in as evidence about folding. If a
candidate changes how often the fold fires, report folds per run beside the
pass rate so a "win" by folding less is visible as that.

**Why measured spend, not token-priced cost.** The session counters record the
main loop's usage. The summarizer's side calls are not in them, so token-priced
cost undercounts. In the first two smoke runs the provider balance fell by
$0.080 while the counters priced the same runs at $0.041: about half the spend
of a folding session is invisible to the counters (measured). Cost per pass
uses the balance delta for that reason. When batches run concurrently the
delta covers all of them together; attribute it per arm only when arms run in
separate batches.

## Arms

- **builtin** (the baseline): current ladder, `SummarySchema::Sections`,
  default thresholds. Nothing overridden except `context_target`.
- **Later arms** plug in by configuration, not by code edits to the harness:
  `loop-ab.sh -A <control overrides> -B <treatment overrides> -C name:<overrides>`
  apply config keys per arm (for example a compaction hook or an addon switched
  on in the treatment only). For I1, a candidate summarizer is a different
  `SummarizeFn` handed to `run_hard_recall_eval_with`.

A flag that ships ON cannot be A/B'd by setting it in the treatment; put the
disable on the control (see the header of `loop-ab.sh`).

## Pairing, sample size, test

- **Pairing.** Arms run on the same pinned binary copy, the same byte-identical
  fixture, the same base config, and the same model. Runs are paired by
  (batch, repeat index). The provider does not honour a sampling seed, so
  "paired seeds" here means paired fixture and position, not identical
  sampling; the pairing removes fixture and drift variance, not model
  sampling variance.
- **Order.** `loop-ab.sh` runs every control repeat before any treatment
  repeat. Run at least two batches concurrently so arms overlap in time, and
  swap `-A`/`-B` between batches when the arms differ, so provider drift over
  the run cannot line up with the arm.
- **n = 20 per arm.** With 20 pairs, the exact sign test reaches p < 0.05 only
  when at least 6 discordant pairs all favour one arm; smaller effects are
  reported as ties, not as directions.
- **Test.** Pass rate: exact two-sided sign test on discordant pairs (McNemar
  exact). Tokens: paired mean delta with a t 95% CI. Cost per pass: reported,
  not tested (it is a ratio of totals).
- **A/A first.** Before trusting any A/B, the baseline is itself run as an A/A
  (two identical arms, which is what the commands below do). The split-half
  difference is the noise floor.
- **Decision rule.** A candidate **ties or beats** the baseline when its pass
  rate is not significantly worse (sign test) *and* its cost per pass is not
  higher, in the fold-chain regime. It **beats** the baseline when, in
  addition, either its pass rate is significantly better or its paired
  input-token delta CI lies wholly below zero. A tie goes to the simpler
  implementation. I1 recall must not regress (mean facts kept and runs losing
  2+ no worse), but a recall win alone is not a win.

## Running it

Build (any toolchain meeting the locked dependencies), then use a minimal base
config so personal MCP servers, hooks and provider routes stay out of every
arm:

```bash
cat > /tmp/base.json <<'EOF'
{
  "provider": "venice",
  "providers": {
    "venice": {
      "provider_type": "openai",
      "base_url": "https://api.venice.ai/api/v1",
      "api_key_env": "VENICE_API_KEY",
      "model": "deepseek-v4-flash",
      "context_window": 1000000
    }
  }
}
EOF

# I2: two concurrent batches, A/A of the baseline, 20 runs in total
for b in 1 2; do
  LOOP_AB_KEEP=1 LOOP_AB_BASE_CONFIG=/tmp/base.json \
    scripts/loop-ab.sh -n 5 -s fold-chain -t 60 > batch-$b.txt 2>&1 &
done; wait

scripts/fold-report.py /tmp/loop-ab.<b1> /tmp/loop-ab.<b2> \
  --price-in 0.138 --price-cached 0.028 --price-out 0.275 \
  --pool builtin --compare control treatment

# I1: live recall, 10 calls per schema arm
cargo test compaction_recall                     # offline layer
DIRGE_BAKEOFF=1 DIRGE_BAKEOFF_PROVIDER=venice \
  DIRGE_BAKEOFF_BASE_URL=https://api.venice.ai/api/v1 \
  DIRGE_BAKEOFF_API_KEY_ENV=VENICE_API_KEY \
  DIRGE_BAKEOFF_MODEL=deepseek-v4-flash DIRGE_BAKEOFF_REPEATS=10 \
  cargo test compaction_bakeoff -- --nocapture --test-threads=1
```

Record the provider balance before and after each batch for measured spend.

## Baseline results

**Status: partial.** The first collection stopped at n = 7 of the planned 20
runs, so the A/A split-half noise floor is not yet established and nothing
below may be used as a decision threshold. Re-run the I2 commands above to
n = 20 before judging any candidate.

I2, fold-chain, builtin arm, deepseek-v4-flash via Venice, `context_target=26000`
(measured; `scripts/fold-report.py` over the kept run directories of two
concurrent batches, pooled):

| Metric | Value |
|---|---|
| runs | 7 (every run folded: 7/7) |
| pass | 4/7 = 0.57, Wilson 95% [0.25, 0.84] |
| passes without a bypass | 1/7 |
| runs that bypassed (re-traversed the chain outside the reads under test) | 4/7 |
| halted by the ladder | 2/7 (other failures 1) |
| folds per run | 2.86 [2.02, 3.69] |
| re-reads per run | 18.9 [4.6, 33.2] |
| turns | 29.4 [22.9, 36.0] |
| input tokens per run | 509k [392k, 627k] (cached 456k) |
| output tokens per run | 7.7k [3.0k, 12.4k] |
| cost per run (token-priced) | $0.0223 [0.0161, 0.0285] |
| cost per pass (token-priced) | $0.039 |

Measured spend was not recorded for this collection; per the note above,
token-priced cost undercounts a folding session by roughly half.

Observation (measured, n = 7): after a fold the model re-reads pages it had
already read and often escapes to a shell traversal of the chain; only one run
in seven passed without that escape. This is the execution-state loss a fold
replacement should reduce, and re-reads per run is the metric that shows it.

I1 recall: not yet run for the baseline.
