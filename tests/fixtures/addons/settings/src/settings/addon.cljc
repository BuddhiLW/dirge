(ns settings.addon
  "Fixture: an addon that reports the settings dirge hands it, at init
   (:dirge/host :config) and at call time (dirge.harness/config)."
  (:require [fixture.addon-protocol :as p]))

(defn- live-settings
  []
  (when-let [f (resolve 'dirge.harness/config)]
    (f "settings")))

(defn- settings-command
  [state]
  (fn [_]
    {:text (str "live=" (pr-str (live-settings))
                " host=" (pr-str (get-in @state [:dirge/host :config])))}))

(defrecord SettingsAddon [state]
  p/IAddon
  (addon-id [_] "settings")
  (initialize! [_ config]
    (reset! state config)
    {:success? true :errors []})
  (shutdown! [_]
    (reset! state nil)
    {:success? true})
  (tools [_] [])
  (hooks [_]
    {:dirge/commands {"settings" {:description "Show this addon's settings"
                                  :handler     (settings-command state)}}})
  (health [_]
    {:status (if @state :ok :down)}))

(defn addon-ctor
  [_config]
  (->SettingsAddon (atom nil)))
