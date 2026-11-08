# lev_classify.janet — typed-question classifier over the lev sidecar.
#
# Mirrors pi's `models.classify(jev, {state, questions})`: ask a System-One
# oracle one or more typed questions about a single state and get back typed
# answers. lev speaks the same wire contract natively (`POST /v1/systemone`),
# so there is no shim — this plugin is the seam between dirge tool calls and
# lev. It lets the agent decide, mid-turn, to classify an issue, a batch of
# comments, or anything else, without a dedicated Rust feature.
#
# Question types (lev-native, plus pi's bool alias):
#   :choice  criteria is a dict of option -> description. Answer carries
#            :choice, :probabilities (option -> p), and :confidence.
#   :score   criteria is an array of 2..10 labels. Answer carries :score (the
#            expected value float, NOT a discrete grade), :legend, the full
#            per-label :probabilities, and :confidence.
#   :noul    criteria optional. Answer carries :noul (p_true) and :confidence.
#   :bool    pi alias for :noul — wire-mapped to `noul` on the way out and
#            mapped back to {:type "bool" :probability p :confidence c} on the
#            way in, so pi-shaped callers are first-class.
#
# `lev/classify` is the testable seam and NEVER throws: on any transport or
# shape error it returns {:stop-reason "error" :error "..."} — pi's
# ClassifierResult contract, where errors are values, not exceptions.
#
# `lev/classify-many` classifies N independent states against one shared
# question set with a bounded number of in-flight HTTP requests. The bound
# lives in Rust (`harness/http-post-many`) because Janet fibers are
# cooperative and `http-post` blocks the worker thread — an `ev/spawn` fan-out
# would SERIALIZE the requests, not overlap them. This is dirge's
# `createLimiter(4)`.
#
# Config (env, read at call time):
#   LEV_URL     base URL of the lev server, default http://127.0.0.1:8080
#   LEV_API_KEY optional; sent as `Authorization: Bearer <key>` when set

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

# Question fields may be keyword-keyed (hand-written Janet) or string-keyed
# (from json-decode, i.e. the tool path). `qtype` reads the type under either
# spelling; `type-key` reports which spelling is present so the wire question
# overrides the SAME key instead of adding a duplicate.
(defn- qtype [q]
  (or (get q :type) (get q "type")))

(defn- type-key [q]
  (if (get q :type) :type "type"))

# Map pi's `bool` question type onto lev's native `noul` wire type. Returns a
# copy so the caller's question table is not mutated.
(defn- wire-question [q]
  (if (= (qtype q) "bool")
    (merge-into @{} q (table (type-key q) "noul"))
    q))

(defn- wire-questions [qs]
  (let [out @{}]
    (each [qid q] (pairs qs)
      (put out qid (wire-question q)))
    out))

# Map lev's `noul` answer back to pi's `bool` shape when the question was a
# bool; every other answer (and every non-bool question) passes through with
# lev's full answer — legend and per-label probabilities included. The bool
# answer uses string keys to match json-decode's shape for pass-through
# answers, so the result is uniformly JSON-shaped.
(defn- normalize-answer [question answer]
  (if (and (= (qtype question) "bool")
           (= (get answer "type") "noul"))
    {"type" "bool"
     "probability" (get answer "noul")
     "confidence" (get answer "confidence")}
    answer))

(defn- normalize-answers [qs answers]
  (let [out @{}]
    (each [qid answer] (pairs answers)
      (put out qid (normalize-answer (get qs qid) answer)))
    out))

(defn lev/classify
  "Ask lev typed questions over a single `state`. Returns
   {:stop-reason \"stop\" :answers {...}} on success, or
   {:stop-reason \"error\" :error \"...\"} on any transport/shape failure.

   `state` is any Janet value (dict/string/number) and is serialized as the
   request `state`. `questions` is a dict of STRING question id -> question
   table {:type \"choice\"|\"score\"|\"noul\"|\"bool\" :instructions \"...\"
   [:criteria ...]}. `cfg` optionally overrides :endpoint / :api-key for
   tests. Never throws."
  [state questions &opt cfg]
  (let [endpoint (or (get cfg :endpoint) (lev-endpoint))
        api-key (or (get cfg :api-key) (lev-api-key))
        qs (if (string? questions) (harness/json-decode questions) questions)
        body (string "{\"state\":" (json-encode state)
                     ",\"questions\":" (json-encode (wire-questions qs)) "}")
        resp (harness/http-post (string endpoint "/v1/systemone") body (lev-headers api-key))
        decoded (if resp (harness/json-decode resp) nil)
        answers (if decoded (get decoded "answers") nil)]
    (if answers
      {:stop-reason "stop" :answers (normalize-answers qs answers)}
      {:stop-reason "error"
       :error "lev unreachable or answered without an `answers` field"})))

(defn lev/classify-many
  "Classify each of `states` against one shared `questions` set with at most
   `:limit` (default 4) requests in flight. Returns an array of lev/classify
   result tables in the same order as `states`. `cfg` optionally overrides
   :endpoint / :api-key / :limit. Never throws."
  [states questions &opt cfg]
  (let [endpoint (or (get cfg :endpoint) (lev-endpoint))
        api-key (or (get cfg :api-key) (lev-api-key))
        limit (or (get cfg :limit) 4)
        qs (if (string? questions) (harness/json-decode questions) questions)
        wire (json-encode (wire-questions qs))
        bodies (map (fn [state]
                      (string "{\"state\":" (json-encode state)
                              ",\"questions\":" wire "}"))
                    states)
        resp (harness/http-post-many (string endpoint "/v1/systemone")
                                     (json-encode bodies)
                                     limit
                                     (lev-headers api-key))
        decoded (if resp (harness/json-decode resp) nil)]
    (if (indexed? decoded)
      (map (fn [body]
             (if (nil? body)
               {:stop-reason "error" :error "request failed"}
               (let [d (harness/json-decode body)
                     answers (if d (get d "answers") nil)]
                 (if answers
                   {:stop-reason "stop" :answers (normalize-answers qs answers)}
                   {:stop-reason "error"
                    :error "lev unreachable or answered without `answers`"}))))
           decoded)
      [{:stop-reason "error" :error "lev unreachable"}])))

# --- LLM-callable tools -----------------------------------------------------

(defn lev-classify-tool-handler [args]
  # `args` is the raw JSON string the LLM produced. Decode it, split the
  # top-level {state, questions}, classify, and re-encode the typed result so
  # the agent reads a clean ClassifierResult-shaped object.
  (let [a (harness/json-decode args)
        state (get a "state")
        questions (get a "questions")]
    (if (and state questions)
      (json-encode (lev/classify state questions))
      (json-encode {:stop-reason "error"
                    :error "lev_classify requires `state` and `questions`"}))))

(defn lev-classify-many-tool-handler [args]
  (let [a (harness/json-decode args)
        states (get a "states")
        questions (get a "questions")
        limit (get a "limit")]
    (if (and (indexed? states) questions)
      (json-encode (lev/classify-many states questions (if limit {:limit limit} @{})))
      (json-encode {:stop-reason "error"
                    :error "lev_classify_many requires `states` (array) and `questions`"}))))

(harness/register-tool
  "lev_classify"
  (string
    "Ask the lev System-One oracle typed questions about a single state and "
    "get back typed answers. `state` is the context to classify (e.g. an issue "
    "or comment object); `questions` maps a string id to {type, instructions, "
    "criteria} where type is one of choice, score, noul, or bool. Use this to "
    "make a calibrated decision (rank, triage, flag) the agent can act on.")
  "lev Classify"
  (string
    "{\"type\":\"object\",\"properties\":{"
    "\"state\":{\"type\":\"object\",\"description\":\"context to classify\"},"
    "\"questions\":{\"type\":\"object\",\"description\":\"id -> {type,instructions,criteria}\"}"
    "},\"required\":[\"state\",\"questions\"]}")
  "lev-classify-tool-handler"
  :parallel)

(harness/register-tool
  "lev_classify_many"
  (string
    "Classify many independent states against one shared question set with "
    "bounded concurrency (max `limit` in flight, default 4). `states` is an "
    "array of context objects; `questions` is the same typed-question map as "
    "lev_classify. Returns an array of ClassifierResult-shaped objects in "
    "order. Prefer this over calling lev_classify in a loop.")
  "lev Classify Many"
  (string
    "{\"type\":\"object\",\"properties\":{"
    "\"states\":{\"type\":\"array\",\"description\":\"contexts to classify\"},"
    "\"questions\":{\"type\":\"object\",\"description\":\"shared id -> {type,instructions,criteria}\"},"
    "\"limit\":{\"type\":\"integer\",\"description\":\"max concurrent requests (default 4)\"}"
    "},\"required\":[\"states\",\"questions\"]}")
  "lev-classify-many-tool-handler"
  :parallel)
