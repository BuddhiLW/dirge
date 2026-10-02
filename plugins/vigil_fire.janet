# vigil_fire.janet — let the agent emit a vigil event mid-turn.
#
# A vigil plugin (requires `--features vigil`, like every other vigil hook):
# registering `vigil_fire` as an LLM-callable tool lets an observance's agent
# turn chain into another vigil by pushing an event into the keeper, instead of
# waiting for the next heartbeat. This is the composition primitive that turns
# "classify something" into "classify, then wake the triage vigil".
#
# The handler is a thin wrapper over `vigil/emit`; it reports whether the
# bridge accepted the event so the agent can tell a no-op (vigil not running)
# from a real emission.

(defn vigil-fire-tool-handler [args]
  # `args` is the raw JSON string the LLM produced: {"name": "...", "payload": {...}}.
  (let [a (harness/json-decode args)
        name (get a "name")
        payload (get a "payload")]
    (cond
      (not (vigil/live?))
        (json-encode {:ok false :error "vigil bridge not active (not in --vigil mode)"})
      (not (string? name))
        (json-encode {:ok false :error "vigil_fire requires a string `name`"})
      (do
        (vigil/emit name payload)
        (json-encode {:ok true :emitted name})))))

(harness/register-tool
  "vigil_fire"
  (string
    "Emit an event into a named vigil from inside the current agent turn. "
    "`name` is the vigil to wake (see vigil/list); `payload` is optional event "
    "context. Use this to chain work: after classifying or fetching, push a "
    "result into the vigil that should react to it.")
  "vigil Fire"
  (string
    "{\"type\":\"object\",\"properties\":{"
    "\"name\":{\"type\":\"string\",\"description\":\"vigil name to emit to\"},"
    "\"payload\":{\"type\":\"object\",\"description\":\"event context data\"}"
    "},\"required\":[\"name\"]}")
  "vigil-fire-tool-handler"
  :sequential)
