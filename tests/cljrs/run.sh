#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Property tests for the cljrs view reducer (src/addons/cljrs/view.cljc),
# run straight on a Clojure engine, without building dirge.
#
#   tests/cljrs/run.sh            # cljrs (default)
#   tests/cljrs/run.sh jvm        # JVM Clojure, against the real test.check
#   tests/cljrs/run.sh all        # both
#
# Environment:
#   CLJRS           cljrs binary (default: `cljrs` on PATH)
#   CLOJURE         clojure CLI (default: `clojure` on PATH)
#   HIVE_TEST_SRC   a hive-test checkout's src dir (default: a cached clone
#                   of HIVE_TEST_SHA)
#
# The addon sources live under src/addons/cljrs/, where the file names do
# not follow the namespace names, so they are staged under dirge/ in a
# temporary source root for the engine to resolve `dirge.view`.
#
# The Rust parity test (src/ui/view/tests.rs) stays the cross-engine gate
# against the native reducer; this suite checks the reducer's own laws.

set -euo pipefail

HIVE_TEST_URL=${HIVE_TEST_URL:-https://github.com/hive-agi/hive-test}
HIVE_TEST_SHA=${HIVE_TEST_SHA:-4d20774cf592966c9ac54fa7fe7a1e2f7d6ae68d}
TEST_NSES=(dirge.view-test)

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)

hive_test_src() {
  if [[ -n "${HIVE_TEST_SRC:-}" ]]; then
    echo "$HIVE_TEST_SRC"
    return
  fi
  local cache="${XDG_CACHE_HOME:-$HOME/.cache}/dirge/hive-test-$HIVE_TEST_SHA"
  if [[ ! -d "$cache/src" ]]; then
    local tmp="$cache.tmp.$$"
    rm -rf "$tmp"
    git init -q "$tmp"
    git -C "$tmp" fetch -q --depth 1 "$HIVE_TEST_URL" "$HIVE_TEST_SHA"
    git -C "$tmp" checkout -q FETCH_HEAD
    mv "$tmp" "$cache"
  fi
  echo "$cache/src"
}

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/dirge"
for f in view panels; do
  ln -s "$repo/src/addons/cljrs/$f.cljc" "$stage/dirge/$f.cljc"
done

ln -s "$here/test" "$stage/test"
ln -s "$(hive_test_src)" "$stage/hive-test"

run_cljrs() {
  echo "== cljrs"
  "${CLJRS:-cljrs}" test \
    --src-path "$stage" \
    --src-path "$stage/test" \
    --src-path "$stage/hive-test" \
    "${TEST_NSES[@]}"
}

run_jvm() {
  echo "== jvm"
  local deps='{:paths ["." "test" "hive-test"]
               :deps {org.clojure/clojure {:mvn/version "1.12.0"}
                      org.clojure/test.check {:mvn/version "1.1.1"}}}'
  local nses="${TEST_NSES[*]}"
  (cd "$stage" && "${CLOJURE:-clojure}" -Sdeps "$deps" -M -e "
(require 'clojure.test)
(def nses '[$nses])
(apply require nses)
(let [r (apply clojure.test/run-tests nses)]
  (System/exit (if (zero? (+ (:fail r) (:error r))) 0 1)))")
}

case "${1:-cljrs}" in
  cljrs) run_cljrs ;;
  jvm) run_jvm ;;
  all) run_cljrs && run_jvm ;;
  *) echo "usage: $0 [cljrs|jvm|all]" >&2; exit 2 ;;
esac
