(ns dirge.addon.host
  "dirge's addon host inside the embedded cljrs runtime. Rust calls
   use-protocol!, load-addon!, call-tool, run-hook and shutdown-all! with
   plain data and reads plain data back."
  (:require [clojure.edn :as edn]))

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
  "The hook keys an addon registered, as strings without the colon."
  [hooks]
  (vec (sort (map (fn [k] (subs (str k) 1)) (keys hooks)))))

(defn index-tools
  "tool-defs keyed by :name."
  [tools]
  (into {} (map (juxt :name identity)) tools))

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
                                                               default-unimplemented?)}))
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

(defn- install!
  [id manifest addon]
  (let [tools (vec ((pf :tools) addon))
        hooks (or (optional :hooks addon {}) {})]
    (swap! !addons assoc id {:addon addon :manifest manifest
                             :tools (index-tools tools) :hooks hooks})
    (swap! !order (fn [ids] (conj (vec (remove #{id} ids)) id)))
    {:id id
     :version (:addon/version manifest)
     :tools (mapv tool-view tools)
     :hooks (hook-names hooks)
     :health (optional :health addon {:status :ok})}))

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

(defn run-hook
  "Call every loaded addon's `hook-key` hook with `ctx`, in load order:
   a vector of {:addon id :ok result} / {:addon id :error msg}."
  [hook-key ctx]
  (let [k (keyword hook-key)]
    (vec
     (for [id @!order
           :let [f (get-in @!addons [id :hooks k])]
           :when f]
       (try
         {:addon id :ok (json-safe (f ctx))}
         (catch #?(:clj Throwable :default :default) t
           (assoc (failure t) :addon id)))))))

(defn shutdown-all!
  "Shut every addon down, newest first."
  []
  (doseq [id (reverse @!order)]
    (shutdown-addon! id))
  true)
