# debounce-adapter — a Janet-side debounce / aggregate / reduce toolkit for
# vigil event producers.
#
# The Rust keeper already debounces at the vigil level (`cooldown_secs`): after
# an observance, further reaps within the window are re-queued. That throttle
# is blunt — it suppresses the whole vigil. This module is the plugin-side
# complement: collapse a burst of raw events *before* calling `(vigil/emit ...)`
# so a noisy producer floods the queue with one summary instead of N raw
# events.
#
# Three verbs, composable:
#   (debounce/coalesce  events key)                -> one event per key
#   (debounce/aggregate events key reducer value)  -> table: key -> value
#   (debounce/reduce    values reducer)            -> single value
#
# `key` / `value` are lookup keys (keywords, e.g. :id) into each event table.
# Read them with `(get ev key)` — do NOT use keyword-call `(:id ev)`, which
# the vendored evil-janet does not treat as an accessor. A call site reads:
#   (debounce/aggregate burst :id :sum :v)

# Collapse a list of values to one. `reducer` is :count, :sum, :latest, or
# :first. Empty :sum is 0; empty :latest/:first are nil.
(defn debounce/reduce [values reducer]
  (case reducer
    :count (length values)
    :sum (reduce + 0 values)
    :latest (last values)
    :first (first values)
    (error (string "debounce/reduce: unknown reducer " reducer))))

# Group events by `(get ev key)`, then reduce each group's `(get ev value)`
# with `reducer`. Returns a table key -> reduced value; flatten it back into
# `(vigil/emit ...)` events, one per key.
(defn debounce/aggregate [events key reducer value]
  (var groups @{})
  (each ev events
    (let [k (get ev key)
          vs (or (get groups k) @[])]
      (array/push vs (get ev value))
      (put groups k vs)))
  (var out @{})
  (each [k vs] (pairs groups)
    (put out k (debounce/reduce vs reducer)))
  out)

# Debounce a burst: keep only the latest event per `(get ev key)`. The
# returned array is in table order, so treat it as a set, not a sequence.
(defn debounce/coalesce [events key]
  (var latest @{})
  (each ev events
    (put latest (get ev key) ev))
  (values latest))
