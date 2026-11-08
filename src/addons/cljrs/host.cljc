(ns dirge.addon.host
  "dirge's addon host inside the embedded cljrs runtime. Rust calls
   use-protocol!, load-addon!, shutdown-addon!, reload-sources!, call-tool,
   run-command, run-hook, emit-fold and shutdown-all! with plain data and
   reads plain data back."
  (:require [clojure.edn :as edn]
            [clojure.string :as str]))

;; SPDX-License-Identifier: GPL-3.0-only

(defn json-safe
  "`x` folded into data a JSON writer can encode."
  [x]
  (cond
    (or (nil? x) (string? x) (boolean? x) (number? x) (keyword? x)) x
    (symbol? x)     (str x)
    (fn? x)         "#fn"
    (map? x)        (into {}
                          (map (fn [[k v]]
                                 [(if (or (string? k) (keyword? k)) k (str k))
                                  (json-safe v)]))
                          x)
    (set? x)        (mapv json-safe (sort-by str x))
    (sequential? x) (mapv json-safe x)
    :else           (str x)))

(def required-fns
  "IAddon functions the protocol namespace must provide."
  '[addon? initialize! shutdown! tools])

(def optional-fns
  "IAddon functions used when the protocol namespace provides them."
  '[hooks health unimplemented-method?])

(def ^:private unimplemented-re
  #"(?i)is abstract|does not define or inherit an implementation|no implementation of (method|protocol)|no protocol method|nothing implements")

(defn- default-unimplemented?
  [t]
  (boolean (some->> (ex-message t) (re-find unimplemented-re))))

(defn failure
  "{:error msg} for a caught throwable."
  [t]
  {:error (or (ex-message t) (str t))})

(defn tool-view
  "What dirge needs of a tool-def: everything but the live :handler."
  [tool]
  (select-keys tool [:name :description :inputSchema]))

(defn hook-names
  "The hook keys an addon registered, as strings without the colon.
   :dirge/commands is the slash-command table, reported as :commands, so
   it is not one of them."
  [hooks]
  (vec (sort (keep (fn [k] (when-not (= k :dirge/commands) (subs (str k) 1)))
                   (keys hooks)))))

(defn index-tools
  "tool-defs keyed by :name."
  [tools]
  (into {} (map (juxt :name identity)) tools))

(defn command-name
  "The name typed after `/` for a :dirge/commands key: its name without
   leading slashes."
  [k]
  (or (re-find #"[^/].*" (name k)) ""))

(defn command-index
  "The slash commands in a hooks map's :dirge/commands entry, keyed by
   command-name: {\"name\" {:description d :class c :handler f}}. Entries
   without a handler are dropped."
  [hooks]
  (into {}
        (for [[k spec] (get hooks :dirge/commands)
              :when (some? (:handler spec))]
          [(command-name k) {:description (or (:description spec) "")
                             :class       (:class spec)
                             :handler     (:handler spec)}])))

(defn command-views
  "What dirge needs of indexed commands: names, descriptions and declared
   classes (a string, or nil when undeclared), sorted."
  [commands]
  (vec (for [[n {:keys [description class]}] (sort-by key commands)]
         {:name n :description description :class (some-> class name)})))

(defn- resolve-fns
  [protocol-ns names]
  (into {} (for [n names] [(keyword n) (resolve (symbol protocol-ns (str n)))])))

(defonce ^:private !protocol (atom nil))
(defonce ^:private !addons (atom {}))
(defonce ^:private !order (atom []))

(defn- pf
  [k]
  (get @!protocol k))

(defn use-protocol!
  "Bind the IAddon functions of `protocol-ns`: {:ok protocol-ns} or {:error msg}."
  [protocol-ns]
  (try
    (require (symbol protocol-ns))
    (let [required (resolve-fns protocol-ns required-fns)
          missing  (sort (for [[k v] required :when (nil? v)] (name k)))]
      (if (seq missing)
        {:error (str protocol-ns " does not define " (apply str (interpose ", " missing)))}
        (let [optional (resolve-fns protocol-ns optional-fns)]
          (reset! !protocol (merge required
                                   (into {} (remove (comp nil? val)) optional)
                                   {:unimplemented-method? (or (:unimplemented-method? optional)
                                                               default-unimplemented?)
                                    :protocol-ns           (symbol protocol-ns)}))
          {:ok protocol-ns})))
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn- optional
  "Call an optional IAddon method, answering `fallback` when the protocol or
   the addon does not provide it."
  [k addon fallback]
  (if-let [method (pf k)]
    (try
      (method addon)
      (catch #?(:clj Throwable :default :default) t
        (if ((pf :unimplemented-method?) t) fallback (throw t))))
    fallback))

(defn- ctor
  "The manifest's constructor fn, loading its namespace first."
  [{:addon/keys [init-ns init-fn]}]
  (require (symbol (str init-ns)))
  (or (resolve (symbol (str init-ns) (str (or init-fn "addon-ctor"))))
      (throw (ex-info (str "init-fn not found: " init-ns "/" init-fn) {}))))

(defn shutdown-addon!
  "Shut one addon down and forget it. Idempotent."
  [id]
  (when-let [{:keys [addon]} (get @!addons id)]
    (try ((pf :shutdown!) addon) (catch #?(:clj Throwable :default :default) _ nil))
    (swap! !addons dissoc id)
    (swap! !order (fn [ids] (vec (remove #{id} ids))))))

(defn- register!
  "Ask `addon` for its tools, hooks and commands now and keep them under
   `id`: the addon's summary."
  [id manifest addon]
  (let [tools    (vec ((pf :tools) addon))
        hooks    (or (optional :hooks addon {}) {})
        commands (command-index hooks)]
    (swap! !addons assoc id {:addon addon :manifest manifest
                             :tools (index-tools tools) :hooks hooks
                             :commands commands})
    {:id id
     :version (:addon/version manifest)
     :tools (mapv tool-view tools)
     :hooks (hook-names hooks)
     :commands (command-views commands)
     :health (optional :health addon {:status :ok})}))

(defn- install!
  [id manifest addon]
  (let [summary (register! id manifest addon)]
    (swap! !order (fn [ids] (conj (vec (remove #{id} ids)) id)))
    summary))

(defn refresh!
  "Ask every loaded addon again for its tools, hooks and commands, without
   shutting it down or initializing it, so definitions changed at a REPL
   take effect: one summary per addon in load order, or {:id id :error msg}
   for an addon that threw and keeps what it registered before."
  []
  (vec
   (for [id @!order
         :let [{:keys [addon manifest]} (get @!addons id)]]
     (try
       (json-safe (register! id manifest addon))
       (catch #?(:clj Throwable :default :default) t
         (assoc (failure t) :id id))))))

(defn load-addon!
  "Load the manifest at `path`: construct, initialize!, register. A manifest
   already loaded under the same id is shut down first, so this is also
   reload. Returns the addon's summary, or {:error msg}."
  [path host-config]
  (try
    (when-not @!protocol
      (throw (ex-info "no IAddon protocol bound; call use-protocol! first" {})))
    (let [manifest (edn/read-string (slurp path))
          id       (:addon/id manifest)
          config   (:addon/config manifest {})]
      (when-not (string? id)
        (throw (ex-info (str "manifest has no string :addon/id: " path) {})))
      (shutdown-addon! id)
      (let [addon ((ctor manifest) config)]
        (when-not ((pf :addon?) addon)
          (throw (ex-info (str id " constructor did not return an IAddon") {})))
        (let [init ((pf :initialize!) addon {:addon/id id
                                             :addon/config config
                                             :dirge/host host-config})]
          (if (false? (:success? init))
            {:error (str id " failed to initialize: " (pr-str (:errors init)))}
            (json-safe (install! id manifest addon))))))
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn call-tool
  "Run `tool-name` of `addon-id` on `params`: {:ok result} or {:error msg}."
  [addon-id tool-name params]
  (try
    (if-let [tool (get-in @!addons [addon-id :tools tool-name])]
      {:ok (json-safe ((:handler tool) params))}
      {:error (str "no tool " tool-name " in addon " addon-id)})
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn run-command
  "Run slash command `command` of `addon-id` with `ctx`: {:ok answer} or
   {:error msg}."
  [addon-id command ctx]
  (try
    (if-let [{:keys [handler]} (get-in @!addons [addon-id :commands command])]
      {:ok (json-safe (handler ctx))}
      {:error (str "no command " command " in addon " addon-id)})
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn run-hook-handler
  "Run command-hook `handler` of `addon-id`, the fn under that name in its
   :dirge/command-hooks entry, with `ctx` ({:payload hook-json}): {:ok answer}
   or {:error msg}. A string handler name also finds a keyword key."
  [addon-id handler ctx]
  (try
    (let [handlers (get-in @!addons [addon-id :hooks :dirge/command-hooks])]
      (if-let [f (or (get handlers handler) (get handlers (keyword handler)))]
        {:ok (json-safe (f ctx))}
        {:error (str "no command hook " handler " in addon " addon-id)}))
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(def keyword-fields-key
  "The ctx key under which an emit site names, as a vector of strings, more
   fields its hook reads as keywords. No hook sees it."
  :dirge/keyword-fields)

(defn keyword-fields
  "`ctx` with each of `fields` that holds a string turned into a keyword."
  [ctx fields]
  (reduce (fn [c field]
            (cond-> c (string? (get c field)) (update field keyword)))
          ctx
          fields))

(defn hook-ctx
  "`ctx` with the fields its emit site declared under `keyword-fields-key`
   turned into keywords, that key removed."
  [_k ctx]
  (keyword-fields (dissoc ctx keyword-fields-key)
                  (map keyword (get ctx keyword-fields-key))))

(defn renamed
  "`m` with each key of `kmap` present in it moved to the key it maps to."
  [m kmap]
  (reduce (fn [m [from to]]
            (if (contains? m from)
              (-> m (assoc to (get m from)) (dissoc from))
              m))
          m
          kmap))

(defmulti event-ctx
  "The `:dirge/event` ctx for one event, given as it serialized (`:event`
   its name as a keyword, its fields the other keys)."
  :event)

(defmethod event-ctx :default [ctx] ctx)

(defmethod event-ctx :tool-call [ctx]
  (renamed ctx {:name :tool}))

(defmethod event-ctx :tool-result [ctx]
  (dissoc ctx :kind))

(defmethod event-ctx :error [ctx]
  (renamed ctx {:value :message}))

(defmethod event-ctx :context-overflow [ctx]
  (-> ctx (dissoc :prompt) (renamed {:error :message})))

(defmethod event-ctx :context-compacted [ctx]
  (-> ctx
      (dissoc :first-kept-index :summary-model)
      (renamed {:new-session-id :session-id :compaction-kind :kind})))

(defmethod event-ctx :checkpoint-refresh [ctx]
  (assoc ctx :event :checkpoint))

(defmethod event-ctx :interjected [ctx]
  (renamed ctx {:partial-response :response}))

(defmethod event-ctx :retry-notice [ctx]
  (-> ctx (assoc :event :retry) (renamed {:error :message})))

(defmethod event-ctx :system-notice [ctx]
  (assoc ctx :event :notice))

(defmethod event-ctx :escalation-activated [ctx]
  (assoc ctx :event :escalation))

(defmulti shape-ctx
  "The ctx the hook keyed `k` hears, given `ctx` as its emit site sent it."
  (fn [k _ctx] k))

(defmethod shape-ctx :default [k ctx]
  (hook-ctx k ctx))

(defmethod shape-ctx :dirge/session-end [k ctx]
  (keyword-fields (hook-ctx k ctx) [:reason]))

(defmethod shape-ctx :dirge/event [k ctx]
  (event-ctx (keyword-fields (hook-ctx k ctx) [:event])))

(defn- answers
  "Every loaded addon's answer to hook `k` on the shaped `ctx`, in load
   order: {:addon id :ok result} or {:addon id :error msg}."
  [k ctx]
  (vec
   (for [id @!order
         :let [f (get-in @!addons [id :hooks k])]
         :when f]
     (try
       {:addon id :ok (json-safe (f ctx))}
       (catch #?(:clj Throwable :default :default) t
         (assoc (failure t) :addon id))))))

(defn run-hook
  "Call every loaded addon's `hook-key` hook with `ctx`, in load order:
   a vector of {:addon id :ok result} / {:addon id :error msg}."
  [hook-key ctx]
  (let [k (keyword hook-key)]
    (answers k (shape-ctx k ctx))))

(defmulti fold-answers
  "One value from the answers `answers` of hook `k`, heard with `ctx`, or
   nil for no answer."
  (fn [k _ctx _answers] k))

(defmethod fold-answers :default [_ _ answers]
  answers)

(defn oks
  "The non-nil :ok values of `answers`, in load order; failures dropped."
  [answers]
  (keep :ok answers))

(defn field
  "`k` of map `m`, whether its key is the keyword or its name."
  [m k]
  (when (map? m)
    (if (contains? m k) (get m k) (get m (name k)))))

(defn- text
  "`s` trimmed, or nil when `s` is not a string or is blank."
  [s]
  (when (string? s)
    (let [t (str/trim s)]
      (when-not (str/blank? t) t))))

(defn- message?
  [m]
  (string? (field m :role)))

(defmethod fold-answers :dirge/transform-context [_ _ answers]
  (some (fn [v]
          (let [ms (field v :messages)]
            (when (and (sequential? ms) (seq ms) (every? message? ms))
              {:messages (vec ms)})))
        (oks answers)))

(def thinking-levels
  "The thinking levels a `:dirge/prepare-next-turn` answer may name."
  #{"off" "minimal" "low" "medium" "high" "xhigh" "max"})

(defn- thinking-level?
  [s]
  (and (string? s) (contains? thinking-levels (str/lower-case (str/trim s)))))

(defn- note
  "An answer's note: a bare string, or a map's :context, trimmed."
  [v]
  (if (map? v) (text (field v :context)) (text v)))

(defmethod fold-answers :dirge/prepare-next-turn [_ _ answers]
  (let [vs       (oks answers)
        level    (fn [v] (let [t (field v :thinking)] (when (thinking-level? t) t)))
        thinking (some level vs)
        notes    (vec (keep note vs))]
    (when (or thinking (seq notes))
      {:thinking thinking :context notes})))

(defn- stop-reason
  "[reason-or-nil] when answer `v` asks to stop the run, else nil."
  [v]
  (if (true? v)
    [nil]
    (let [s (field v :stop)]
      (cond
        (true? s) [nil]
        (text s)  [(text s)]
        :else     nil))))

(defmethod fold-answers :dirge/should-stop-after-turn [_ _ answers]
  (some (fn [a]
          (when-let [[reason] (and (contains? a :ok) (stop-reason (:ok a)))]
            {:addon (:addon a) :reason reason}))
        answers))

(defmethod fold-answers :dirge/acp-ext-method [_ _ answers]
  (first (oks answers)))

(defn- key-name
  [k]
  (if (keyword? k) (subs (str k) 1) (str k)))

(defn- string-keys
  [m]
  (into {} (map (fn [[k v]] [(key-name k) v])) m))

(defmethod fold-answers :dirge/acp-meta [_ ctx answers]
  (let [maps (cons (or (field ctx :response-meta) {})
                   (filter map? (oks answers)))]
    (not-empty (reduce (fn [acc m] (merge (string-keys m) acc)) {} maps))))

(defn emit-fold
  "Run hook `hook-key` on `ctx` and fold its answers: {:value v :failed
   [{:addon id :error msg}]}, v json-safe and nil for no answer."
  [hook-key ctx]
  (let [k       (keyword hook-key)
        ctx     (shape-ctx k ctx)
        replies (answers k ctx)]
    {:value  (json-safe (fold-answers k ctx replies))
     :failed (filterv :error replies)}))

(defn- load-source!
  "load-file `path`, leaving *ns* where it was: nil, or {:error msg}."
  [path]
  (let [saved  (ns-name *ns*)
        result (try
                 (load-file path)
                 nil
                 (catch #?(:clj Throwable :default :default) t
                   (failure t)))]
    (in-ns saved)
    result))

(defn- require-targets
  "The namespaces one :require or :use spec names: a symbol, a vector
   headed by one, or a prefix list."
  [spec]
  (cond
    (symbol? spec) [spec]
    (vector? spec) (let [lib (first spec)] (when (symbol? lib) [lib]))
    (seq? spec)    (let [prefix (first spec)]
                     (for [lib  (rest spec)
                           :let [lib (if (vector? lib) (first lib) lib)]
                           :when (symbol? lib)]
                       (symbol (str prefix "." lib))))
    :else          nil))

(defn ns-deps
  "What an ns form declares: {:ns name :requires #{name}}, :requires being
   the namespaces its :require and :use clauses name. nil for any other
   form."
  [form]
  (when (and (seq? form) (= 'ns (first form)) (symbol? (second form)))
    {:ns       (second form)
     :requires (set (for [clause (drop 2 form)
                          :when  (and (seq? clause)
                                      (contains? #{:require :use} (first clause)))
                          spec   (rest clause)
                          lib    (require-targets spec)]
                      lib))}))

(defn load-order
  "`sources` ({:ns name :requires #{name}}) ordered so each follows the
   sources it requires, ties in input order: {:order [source] :cycle
   [source]}. :cycle holds, in input order, the sources no order can place:
   those in a require cycle and those requiring one."
  [sources]
  (let [known (set (map :ns sources))]
    (loop [order [] pending (vec sources)]
      (let [placed (set (map :ns order))
            ready? (fn [{:keys [ns requires]}]
                     (every? #(or (= % ns) (contains? placed %) (not (contains? known %)))
                             requires))
            ready  (filterv ready? pending)]
        (if (empty? ready)
          {:order order :cycle pending}
          (recur (into order ready) (vec (remove ready? pending))))))))

(defn- first-form
  "The first form of the source at `path`, or nil when it cannot be read."
  [path]
  (try
    (read-string (slurp path))
    (catch #?(:clj Throwable :default :default) _ nil)))

(defn- source-info
  "`source` ({:file path :ns name-or-nil}) as reloading reads it: :ns the
   namespace its ns form declares, else the one its path names; :by-path
   the latter; :requires what its ns form requires."
  [{:keys [file ns]}]
  (let [by-path  (when ns (symbol ns))
        declared (ns-deps (first-form file))]
    {:file     file
     :ns       (or (:ns declared) by-path)
     :by-path  by-path
     :requires (or (:requires declared) #{})}))

(defn- loaded?
  [ns]
  (boolean (and ns (find-ns ns))))

(defn- evaluate!
  "load-file each of `files` in order, retrying the ones that fail while a
   pass makes progress, since one may need a definition a later file adds.
   Answers [{:file path :error msg}] for those still failing."
  [files]
  (loop [pending (vec files)]
    (let [failed (vec (keep (fn [f]
                              (when-let [e (load-source! f)]
                                (assoc e :file f)))
                            pending))]
      (if (or (empty? failed) (= (count failed) (count pending)))
        failed
        (recur (mapv :file failed))))))

(defn- unloaded-row
  "Why `source`, whose namespace nothing has loaded, was not evaluated:
   {:file :error} when `require` could never load it from its path, else
   {:file :skipped}."
  [{:keys [file ns by-path]}]
  (cond
    (nil? ns)
    {:file file :error "not reloaded: no ns form, and outside every source root"}

    (and by-path (not= ns by-path))
    {:file file :error (str "not reloaded: declares " ns " but its path names " by-path)}

    :else
    {:file file :skipped (str "not reloaded: nothing has loaded " ns)}))

(defn reload-sources!
  "Evaluate again every source in `sources` ({:file path :ns name-or-nil})
   whose namespace is loaded, each after the sources it requires, so it runs
   the code now on disk. A source's namespace is the one its ns form
   declares, else `:ns`. The bound IAddon protocol namespace is never
   evaluated again. Answers one row per source not brought up to date:
   {:file path :error msg} for a load failure, a require cycle, or a file
   `require` could never load; {:file path :skipped msg} for the protocol
   namespace and for namespaces nothing has loaded."
  [sources]
  (let [infos                 (mapv source-info sources)
        protocol-ns           (pf :protocol-ns)
        protocol?             (fn [s] (and (some? protocol-ns) (= (:ns s) protocol-ns)))
        live                  (filterv #(and (loaded? (:ns %)) (not (protocol? %))) infos)
        {:keys [order cycle]} (load-order live)
        failed                (evaluate! (map :file order))
        cyclic                (apply str (interpose ", " (sort (distinct (map (comp str :ns) cycle)))))
        dormant               (filterv #(not (or (protocol? %) (loaded? (:ns %)))) infos)]
    (vec (concat
          failed
          (for [{:keys [file]} cycle]
            {:file file :error (str "not reloaded: require cycle among " cyclic)})
          (for [{:keys [file]} (filter protocol? infos)]
            {:file file :skipped (str "not reloaded: " protocol-ns " is the IAddon protocol namespace")})
          (map unloaded-row dormant)))))

(defn shutdown-all!
  "Shut every addon down, newest first."
  []
  (doseq [id (reverse @!order)]
    (shutdown-addon! id))
  true)
