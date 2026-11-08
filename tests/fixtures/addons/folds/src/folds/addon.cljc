(ns folds.addon
  "An addon whose tools run the addon host's folds and shapes on the data
   they are given: `fold` runs fold-answers, `shape` runs shape-ctx. Each
   answers {:result r}."
  (:require [fixture.addon-protocol :as p]))

(defn- host
  [f]
  (resolve (symbol "dirge.addon.host" f)))

(defn- fold
  [{:keys [key ctx answers]}]
  {:result ((host "fold-answers") (keyword key) (or ctx {}) (vec answers))})

(defn- shape
  [{:keys [key ctx]}]
  {:result ((host "shape-ctx") (keyword key) (or ctx {}))})

(defrecord FoldsAddon []
  p/IAddon
  (addon-id [_] "folds")
  (initialize! [_ _config]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    [{:name "fold"
      :description "fold-answers of `key` over `ctx` and `answers`"
      :inputSchema {:type "object"}
      :handler fold}
     {:name "shape"
      :description "shape-ctx of `key` over `ctx`"
      :inputSchema {:type "object"}
      :handler shape}])
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->FoldsAddon))
