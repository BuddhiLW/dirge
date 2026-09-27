(ns echo.addon
  (:require [fixture.addon-protocol :as p]))

(defn- notify!
  [msg]
  (when-let [f (resolve 'dirge.harness/notify)]
    (f msg :info)))

(defn- count-rows
  [params]
  {:content [{:type "text" :text (str "rows=" (count (:rows params)))}]})

(defrecord EchoAddon [state]
  p/IAddon
  (addon-id [_] "echo")
  (initialize! [_ config]
    (reset! state config)
    (notify! "echo loaded")
    {:success? true :errors []})
  (shutdown! [_]
    (reset! state nil)
    {:success? true})
  (tools [_]
    [{:name        "count-rows"
      :description "Counts the rows it is handed"
      :inputSchema {:type "object" :properties {:rows {:type "array"}}}
      :handler     count-rows}])
  (hooks [_]
    {:dirge/system-prompt (fn [_] "echo addon active")})
  (health [_]
    {:status (if @state :ok :down)}))

(defn addon-ctor
  [_config]
  (->EchoAddon (atom nil)))
