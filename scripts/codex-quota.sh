#!/usr/bin/env bash
set -euo pipefail

# codex-quota.sh — snapshot the ChatGPT/Codex subscription quota headers.
#
# A subscription has no balance to poll, so the context-economy baseline on
# the Codex OAuth route (docs/context-economy-baseline.md) records quota
# consumption instead: the x-codex-* headers returned on a minimal /responses
# call (plan, primary/secondary window used-percent and reset time). Take one
# snapshot before and one after a batch; the used-percent delta is the batch's
# quota cost. The probe itself is one tiny request and is counted in it.
#
# Reads the token from ~/.codex/auth.json ($CODEX_HOME honoured), never prints
# it, and drops the opaque x-codex-turn-state header. Output: one line,
# `<iso-time> key=value ...`, appended to $1 when given.
#
# Usage: scripts/codex-quota.sh [OUTFILE] [MODEL]

out="${1:-}"
model="${2:-gpt-6-sol}"
auth="${CODEX_HOME:-$HOME/.codex}/auth.json"
[ -f "$auth" ] || { echo "error: no Codex auth at $auth" >&2; exit 1; }
hdr="$(mktemp)"
trap 'rm -f "$hdr"' EXIT

jq -r '"Authorization: Bearer \(.tokens.access_token)\nchatgpt-account-id: \(.tokens.account_id)"' "$auth" |
  timeout 60 curl -s -o /dev/null -D "$hdr" -H @- \
    -H 'Content-Type: application/json' -H 'originator: codex_cli_rs' \
    https://chatgpt.com/backend-api/codex/responses \
    -d "{\"model\":\"$model\",\"instructions\":\"Reply with OK.\",\"input\":[{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"ping\"}]}],\"store\":false,\"stream\":true}"

status="$(head -1 "$hdr" | awk '{print $2}')"
fields="$(grep -iE '^(x-codex-|x-ratelimit-|retry-after)' "$hdr" |
  grep -viE '^x-codex-turn-state' | tr -d '\r' |
  sed -E 's/^([^:]+):[[:space:]]*/\1=/' | sort | tr '\n' ' ')"
line="$(date -Is) status=$status ${fields% }"
if [ -n "$out" ]; then printf '%s\n' "$line" >> "$out"; fi
printf '%s\n' "$line"
