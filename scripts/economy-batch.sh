#!/usr/bin/env bash
set -euo pipefail

# economy-batch.sh — one I2 fold-chain batch of the context-economy baseline
# (docs/context-economy-baseline.md), with quota snapshots around it.
#
# Runs `loop-ab.sh -s fold-chain` A/A against a pinned binary and base config,
# keeps the artefacts, and records the Codex subscription quota headers before
# and after (scripts/codex-quota.sh), since a subscription has no balance.
#
# Usage: scripts/economy-batch.sh <label> <repeats> <base-config> [binary]
#   output: runs/<label>/{batch.txt,quota.log,workdir} — workdir holds the
#   path of the kept loop-ab artefact directory (TMPDIR=runs/<label>/tmp).

label="$1"; repeats="$2"; base="$3"; bin="${4:-target/release/dirge}"
root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/runs/$label"
mkdir -p "$dir/tmp"
cd "$root"
scripts/codex-quota.sh "$dir/quota.log" >/dev/null || echo "quota probe failed (before)" >> "$dir/quota.log"
date -Is > "$dir/started"
TMPDIR="$dir/tmp" LOOP_AB_KEEP=1 LOOP_AB_BASE_CONFIG="$base" \
  scripts/loop-ab.sh -b "$bin" -n "$repeats" -s fold-chain -t 60 > "$dir/batch.txt" 2>&1 || true
ls -d "$dir"/tmp/loop-ab.* > "$dir/workdir" 2>/dev/null || true
date -Is > "$dir/finished"
scripts/codex-quota.sh "$dir/quota.log" >/dev/null || echo "quota probe failed (after)" >> "$dir/quota.log"
