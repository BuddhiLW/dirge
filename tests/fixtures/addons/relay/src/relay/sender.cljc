(ns relay.sender
  "Addon A: its `send` tool emits `key` through `dirge.harness/emit` and
   answers what emit said. Its `:acme/echo` listener tries to emit from
   inside a posted call, which must be refused."
  (:require [fixture.addon-protocol :as p]))

(defonce !nested (atom nil))

(defn- emit!
  ([k] ((resolve (symbol "dirge.harness" "emit")) k))
  ([k payload] ((resolve (symbol "dirge.harness" "emit")) k payload)))

(defn- text
  [s]
  {:content [{:type "text" :text s}]})

(defn- send-tool
  [{:keys [key payload]}]
  (let [answer (if (some? payload) (emit! key payload) (emit! key))]
    (text (if (:ok answer) "ok" (str "error: " (:error answer))))))

(defrecord Sender []
  p/IAddon
  (addon-id [_] "sender")
  (initialize! [_ _config]
    {:success? true :errors []})
  (shutdown! [_]
    {:success? true})
  (tools [_]
    [{:name "send"
      :description "Emits `key` with `payload`"
      :inputSchema {:type "object"}
      :handler send-tool}
     {:name "nested"
      :description "What the emit inside a posted call answered"
      :inputSchema {:type "object"}
      :handler (fn [_] (text (let [a @!nested]
                               (cond (nil? a) "none"
                                     (:ok a) "ok"
                                     :else (str "error: " (:error a))))))}])
  (hooks [_]
    {:acme/echo (fn [_] (reset! !nested (emit! :acme/x {:n 99})) nil)})
  (health [_]
    {:status :ok}))

(defn addon-ctor
  [_config]
  (->Sender))
