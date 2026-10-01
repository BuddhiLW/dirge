(ns keywords.addon
  (:require [fixture.addon-protocol :as p]))

(defn- kinds
  "What each value of `ctx` arrived as: keyword, string or other."
  [ctx]
  (into {} (for [[k v] ctx]
             [k (cond (keyword? v) "keyword"
                      (string? v)  "string"
                      :else        "other")])))

(defrecord KeywordsAddon []
  p/IAddon
  (addon-id [_] "keywords")
  (initialize! [_ _config]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_] [])
  (hooks [_]
    {:fixture/probe     kinds
     :dirge/session-end kinds})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->KeywordsAddon))
