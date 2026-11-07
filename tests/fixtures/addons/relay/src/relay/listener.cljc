(ns relay.listener
  "Addon B: hears `:acme/x` and keeps the `:n` of every payload it got."
  (:require [fixture.addon-protocol :as p]))

(defonce !got (atom []))

(defn- text
  [s]
  {:content [{:type "text" :text s}]})

(defrecord Listener []
  p/IAddon
  (addon-id [_] "listener")
  (initialize! [_ _config]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    [{:name "got"
      :description "The :n of each :acme/x payload heard so far"
      :inputSchema {:type "object"}
      :handler (fn [_] (text (apply str (interpose "," (map :n @!got)))))}])
  (hooks [_]
    {:acme/x (fn [ctx] (swap! !got conj ctx) nil)})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->Listener))
