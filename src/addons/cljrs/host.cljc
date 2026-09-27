(ns dirge.addon.host
  "dirge's IAddon host inside the embedded cljrs runtime. Rust calls
   load-addon!, call-tool, run-hook and shutdown-all! with plain data and
   reads back values folded by hive-addon.wire/json-safe."
  (:require [clojure.edn :as edn]
            [hive-addon.protocol :as p]
            [hive-addon.wire :as wire]))

;; SPDX-License-Identifier: GPL-3.0-only

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

(defn failure
  "{:error msg} for a caught throwable."
  [t]
  {:error (or (ex-message t) (str t))})

(defonce ^:private !addons (atom {}))
(defonce ^:private !order (atom []))

(defn- optional
  "Call an optional IAddon method, answering `fallback` when the addon does
   not implement it (the registry contract for excluded-tools and hooks)."
  [method addon fallback]
  (try
    (method addon)
    (catch #?(:clj Throwable :default :default) t
      (if (p/unimplemented-method? t) fallback (throw t)))))

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
    (try (p/shutdown! addon) (catch #?(:clj Throwable :default :default) _ nil))
    (swap! !addons dissoc id)
    (swap! !order (fn [ids] (vec (remove #{id} ids))))))

(defn- install!
  [id manifest addon]
  (let [tools (vec (p/tools addon))
        hooks (or (optional p/hooks addon {}) {})]
    (swap! !addons assoc id {:addon addon :manifest manifest
                             :tools (index-tools tools) :hooks hooks})
    (swap! !order (fn [ids] (conj (vec (remove #{id} ids)) id)))
    {:id id
     :version (:addon/version manifest)
     :tools (mapv tool-view tools)
     :hooks (hook-names hooks)
     :health (optional p/health addon {:status :ok})}))

(defn load-addon!
  "Load the manifest at `path`: construct, initialize!, register. A manifest
   already loaded under the same id is shut down first, so this is also
   reload. Returns the addon's summary, or {:error msg}."
  [path host-config]
  (try
    (let [manifest (edn/read-string (slurp path))
          id       (:addon/id manifest)
          config   (:addon/config manifest {})]
      (when-not (string? id)
        (throw (ex-info (str "manifest has no string :addon/id: " path) {})))
      (shutdown-addon! id)
      (let [addon ((ctor manifest) config)]
        (when-not (p/addon? addon)
          (throw (ex-info (str id " constructor did not return an IAddon") {})))
        (let [init (p/initialize! addon {:addon/id id
                                         :addon/config config
                                         :dirge/host host-config})]
          (if (false? (:success? init))
            {:error (str id " failed to initialize: " (pr-str (:errors init)))}
            (wire/json-safe (install! id manifest addon))))))
    (catch #?(:clj Throwable :default :default) t
      (failure t))))

(defn call-tool
  "Run `tool-name` of `addon-id` on `params`: {:ok result} or {:error msg}."
  [addon-id tool-name params]
  (try
    (if-let [tool (get-in @!addons [addon-id :tools tool-name])]
      {:ok (wire/json-safe ((:handler tool) params))}
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
         {:addon id :ok (wire/json-safe (f ctx))}
         (catch #?(:clj Throwable :default :default) t
           (assoc (failure t) :addon id)))))))

(defn shutdown-all!
  "Shut every addon down, newest first."
  []
  (doseq [id (reverse @!order)]
    (shutdown-addon! id))
  true)
