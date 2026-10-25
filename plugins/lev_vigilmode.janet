# lev_vigilmode.janet — Level 1 end-to-end "vigilmode" composition.
#
# This is the acceptance seam for slice 06: the Earendil demo, expressed in
# dirge's native system. The agent composes primitives it now owns —
# `harness/http-get` (fetch) -> `lev/classify-many` (bounded fan-out) ->
# `harness/store` (transient stash) -> observance summarizes the top N.
#
# `lev/vigilmode` bundles the fan-out + store + rank steps so the whole
# pipeline is one tool call (the "slash command" E names), while the fetch
# stays a separate `harness/http-get` the agent performs first. That keeps the
# issue source swappable (GitHub API, a local file, anything http-get can
# reach) without touching this plugin.
#
# The rank key is the FIRST question in `questions`, unless `cfg` carries an
# explicit `:rank` override. Its `score` answer orders `:ranked` descending.
# Items that answer without that score sink to the bottom (sentinel
# -1000000000). The full unordered verdict list is stashed under the session
# key "verdicts" via `harness/store` AND returned as `:verdicts`, so the agent
# can either re-rank or summarize directly.
#
# Depends on plugins/lev_classify.janet, which the directory loader reads
# first (alphabetical order: lev_classify < lev_vigilmode). Never throws.

(defn- rank-key [questions cfg]
  (or (get cfg :rank) (first (keys questions))))

(defn- score-of [result qid]
  (or (get-in result [:answers qid "score"]) -1000000000))

(defn- rank [items results qid]
  (let [scored (map (fn [i]
                      {:item (get items i)
                       :score (score-of (get results i) qid)})
                    (range (length items)))]
    (sort scored (fn [a b] (> (get a :score) (get b :score))))))

(defn lev/vigilmode
  "Classify `items` (an array of state objects) against a shared typed
   `questions` set with bounded fan-out, stash the full verdicts under the
   session key \"verdicts\", and return {:verdicts [...] :ranked [...]} where
   :ranked is the top `:top-n` (default 5) items by the rank score question
   (`:rank` in cfg, else the first question), descending. `cfg` optionally
   overrides :endpoint / :api-key / :limit / :top-n / :rank. Never throws."
  [items questions &opt cfg]
  (let [qs (if (string? questions) (harness/json-decode questions) questions)
        qid (rank-key qs cfg)
        results (lev/classify-many items qs cfg)
        top-n (or (get cfg :top-n) 5)]
    (harness/store "verdicts" (json-encode results))
    {:verdicts results
     :ranked (take top-n (rank items results qid))}))

(defn lev-vigilmode-tool-handler [args]
  (let [a (harness/json-decode args)
        items (get a "items")
        questions (get a "questions")
        top-n (get a "top_n")]
    (if (and (indexed? items) questions)
      (json-encode (lev/vigilmode items questions (if top-n {:top-n top-n} @{})))
      (json-encode {:stop-reason "error"
                    :error "lev_vigilmode requires `items` (array) and `questions`"}))))

(harness/register-tool
  "lev_vigilmode"
  (string
    "Run the end-to-end lev demo: classify an array of `items` (issue/comment "
    "objects) against a typed `questions` set with bounded concurrency, stash "
    "the verdicts, and return the top `top_n` (default 5) ranked by the first "
    "score question. Fetch the items first with harness/http-get, then pass "
    "them here.")
  "lev Vigilmode"
  (string
    "{\"type\":\"object\",\"properties\":{"
    "\"items\":{\"type\":\"array\",\"description\":\"state objects to classify and rank\"},"
    "\"questions\":{\"type\":\"object\",\"description\":\"typed question set; the first score question is the rank key\"},"
    "\"top_n\":{\"type\":\"integer\",\"description\":\"how many top items to return (default 5)\"}"
    "},\"required\":[\"items\",\"questions\"]}")
  "lev-vigilmode-tool-handler"
  :parallel)
