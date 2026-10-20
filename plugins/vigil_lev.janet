# vigil_lev.janet — lev rite gate for the vigil feature line.
#
# System-1 gate ahead of a vigil observance: before the agent is woken for a
# toll/watcher/harbinger reap, this hook POSTs the coalesced event state to a
# lev sidecar (`POST /v1/systemone`) and asks one yes/no question — "does the
# current state require agent intervention?". lev answers with a `noul`
# probability (0..1). The hook reports that signal to the host via
# `harness/verdict`; the host compares it to the vigil's wake threshold
# (derived from the `gate.policy` cost matrix, default 0.8) and rouses or
# shrouds. The plugin holds no threshold of its own — if lev is unreachable or
# answers without a numeric noul, the hook reports nothing and the vigil's
# fail posture (default open) applies.
#
# Config (env, read at call time):
#   LEV_URL     base URL of the lev server, default http://127.0.0.1:8080
#   LEV_API_KEY optional; sent as `Authorization: Bearer <key>` when set
#
# `lev-verdict` is the testable seam: it returns lev's `noul` probability as a
# number, or nil when lev is unreachable/malformed. It accepts an optional cfg
# table (:endpoint / :api-key) so tests can point it at a mock lev without
# touching process env vars.

(def hooks ["on-vigil-rite"])

(defn- lev-endpoint []
  (or (os/getenv "LEV_URL") "http://127.0.0.1:8080"))

(defn- lev-api-key []
  (os/getenv "LEV_API_KEY"))

# Assumes the key holds no `"` or `\` (true of lev's tokens); a pathological
# key would need JSON escaping here.
(defn- lev-headers [api-key]
  (if api-key
    (string "{\"Authorization\":\"Bearer " api-key "\"}")
    ""))

(defn lev-verdict
  "POST the vigil state to lev and return its `noul` probability, or nil when
   lev is unreachable or answers without a numeric noul. `ctx` is the
   on-vigil-rite context (:vigil :trigger :event_count :payload :threshold);
   `cfg` optionally overrides :endpoint / :api-key for tests."
  [ctx &opt cfg]
  (let [endpoint (or (get cfg :endpoint) (lev-endpoint))
        api-key (or (get cfg :api-key) (lev-api-key))
        state (or (get ctx :payload) "{}")
        body (string
               "{\"state\":" state
               ",\"questions\":{\"judgment\":{"
               "\"type\":\"noul\","
               "\"instructions\":\"The current state requires agent intervention.\""
               "}}}")
        resp (harness/http-post (string endpoint "/v1/systemone") body (lev-headers api-key))
        decoded (if resp (harness/json-decode resp) nil)
        prob (if decoded (get-in decoded ["answers" "judgment" "noul"]) nil)]
    (if (number? prob) prob nil)))

(defn on-vigil-rite [ctx]
  (when-let [prob (lev-verdict ctx)]
    (harness/verdict prob)))
