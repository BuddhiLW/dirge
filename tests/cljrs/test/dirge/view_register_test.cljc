(ns dirge.view-register-test
  "Laws of addon registration in `dirge.view`: registering a view command
   or grid key adds it to the open set the model lists, and a built-in is
   never overridden (built-in wins, the refusal is a warning notice).
   Run by tests/cljrs/run.sh on cljrs and on the JVM."
  ;; :default stays last: it matches every host, so a branch after it is dead.
  (:require [clojure.test :refer [deftest is testing]]
            #?@(:clj [[clojure.test.check.clojure-test :refer [defspec]]
                      [clojure.test.check.properties :as prop]
                      [clojure.test.check.generators :as gen]]
                :default [[hive-test.tcheck.clojure-test :refer [defspec]]
                          [hive-test.tcheck.properties :as prop]
                          [hive-test.tcheck.generators :as gen]])
            [dirge.panels :as panels]
            [dirge.view :as view]))

;; SPDX-License-Identifier: GPL-3.0-only

(def closed
  {:swarm nil
   :panels panels/empty-state
   :producer {:replies [] :keys [] :usage ""}})

(defn opened
  [selected]
  (assoc closed :swarm {:selected selected}))

(defn grid-event
  [cells columns key]
  {:type "grid" :cells cells :columns columns :key key})

(def empty-registry {:commands {} :grid-keymap {} :grid-verbs {} :warnings []})

(defn fresh-registry!
  []
  (reset! view/registry empty-registry))

(def built-in-names (vec (sort (keys view/commands))))
(def built-in-keys (vec (sort (keys view/grid-keymap))))

(def gen-addon-name
  "Command names: fresh ones and dirge's own."
  (gen/one-of [(gen/fmap #(str "addon-" %) (gen/choose 0 1000))
               (gen/elements built-in-names)]))

(def gen-cells
  (gen/fmap (fn [kinds]
              (vec (map-indexed (fn [i k] {:kind k :id (str k "-" i)}) kinds)))
            (gen/vector (gen/elements ["agent" "panel"]) 1 8)))

(def gen-word
  (gen/elements ["" "on" "off" "toggle" "left|right" "debug" "next"]))

(defn hello-run
  [state args]
  [state [(view/notify :info (apply str "hello" (map #(str " " %) args)))]])

(defspec registering-a-command-then-listing-returns-it 200
  (prop/for-all [names (gen/vector gen-addon-name 1 6)]
    (fresh-registry!)
    (doseq [n names]
      (view/register-view-command! n {:run hello-run :summary (str "addon " n)}))
    (let [listed (:view_commands (view/model closed))
          want   (vec (sort (distinct (concat built-in-names names))))]
      (and (= want (mapv :name listed))
           (every? (fn [n]
                     (let [c (first (filter #(= n (:name %)) listed))]
                       (if (contains? view/commands n)
                         (= (:summary (get view/commands n)) (:summary c))
                         (= (str "addon " n) (:summary c)))))
                   names)))))

(defspec a-built-in-command-is-never-overridden 200
  (prop/for-all [name (gen/elements built-in-names)
                 args (gen/vector gen-word 0 2)]
    (fresh-registry!)
    (let [answer (view/register-view-command!
                  name {:run (fn [s _] [s [(view/notify :info "hijacked")]])})
          event  {:type "command" :name name :args args}]
      (and (= :refused (first answer))
           (= (view/step closed event)
              ((:run (get view/commands name)) closed args))
           (= 1 (count (view/drain-warnings!)))
           (empty? (view/drain-warnings!))))))

(defspec a-built-in-grid-key-is-never-overridden 200
  (prop/for-all [key (gen/elements built-in-keys)
                 cells gen-cells
                 columns (gen/choose 0 5)]
    (fresh-registry!)
    (let [state (opened nil)
          event (grid-event cells columns key)
          want  (view/step state event)]
      (view/register-grid-key! key (fn [s _] [(assoc s :swarm nil) [(view/notify :info "hijacked")]]))
      (= want (view/step state event)))))

(deftest a-registered-command-runs-and-a-registered-key-runs-on-the-grid
  (fresh-registry!)
  (is (= [:ok "hello"] (view/register-view-command! "hello" {:run hello-run})))
  (is (= [closed [{:op :notify :level :info :text "hello a b"}]]
         (view/step closed {:type "command" :name "hello" :args ["a" "b"]})))
  (testing "a throwing command leaves the state and reports"
    (view/register-view-command! "boom" {:run (fn [_ _] (throw (ex-info "kaboom" {})))})
    (let [[s effects] (view/step closed {:type "command" :name "boom" :args []})]
      (is (= closed s))
      (is (= :error (:level (first effects))))))
  (testing "a malformed registration is refused"
    (is (= :refused (first (view/register-view-command! "" {:run hello-run}))))
    (is (= :refused (first (view/register-view-command! "nofn" {}))))
    (is (= 2 (count (view/drain-warnings!)))))
  (testing "a registered grid key"
    (is (= [:ok "x"] (view/register-grid-key! "x" (fn [s g] [s [(view/notify :info (str (:n g)))]]))))
    (is (some #{"x"} (:grid_keys (view/model closed))))
    (is (= [{:op :notify :level :info :text "2"}]
           (second (view/step (opened nil)
                              (grid-event [{:kind "agent" :id "a"} {:kind "agent" :id "b"}] 1 "x"))))))
  (fresh-registry!))
