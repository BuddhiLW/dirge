(ns dirge.view-test
  "Laws of the view reducer `dirge.view/step`, checked as properties.

   `step` is pure, [state event] -> [state' effects], so each law is stated
   over generated states and events rather than over hand-picked cases.
   Run by tests/cljrs/run.sh on cljrs and on the JVM."
  ;; :default stays last: it matches every host, so a branch after it is dead.
  (:require [clojure.test :refer [deftest is testing]]
            #?@(:clj [[clojure.test.check.clojure-test :refer [defspec]]
                      [clojure.test.check.properties :as prop]
                      [clojure.test.check.generators :as gen]]
                :default [[hive-test.tcheck.clojure-test :refer [defspec]]
                          [hive-test.tcheck.properties :as prop]
                          [hive-test.tcheck.generators :as gen]])
            [hive-test.properties :refer [defprop-total defprop-invariant]]
            [dirge.panels :as panels]
            [dirge.view :as view]))

;; SPDX-License-Identifier: GPL-3.0-only

;; ---------------------------------------------------------------------------
;; Domain values

(def closed
  "The view state before anything happened: the grid closed."
  {:swarm nil
   :panels panels/empty-state
   :producer {:replies [] :keys [] :usage ""}})

(defn opened
  "The grid open with `selected` as the selected cell."
  [selected]
  (assoc closed :swarm {:selected selected}))

(def gone-cell
  "A cell that is never among the generated cells: a finished subagent."
  {:kind "agent" :id "gone"})

(defn grid-event
  [cells columns key]
  {:type "grid" :cells cells :columns columns :key key})

(defn verb-of
  "The dirge verb a grid key runs, or nil when the key is the producer's."
  [key]
  (first (get view/grid-keymap key)))

(defn distance
  [a b]
  (if (< a b) (- b a) (- a b)))

(defn index-in
  [cells cell]
  (first (keep-indexed (fn [i c] (when (= c cell) i)) cells)))

(defn cursor
  "Where the grid cursor sits: the selected cell, else the first."
  [cells selected]
  (or (index-in cells selected) 0))

(defn selected-of
  [state]
  (get-in state [:swarm :selected]))

(defn grid-open?
  [state]
  (some? (:swarm state)))

;; ---------------------------------------------------------------------------
;; Generators

(def gen-kind (gen/elements ["agent" "panel"]))

(def gen-cells
  "Distinct cells in paint order; the index in the id keeps them apart."
  (gen/fmap (fn [kinds]
              (vec (map-indexed (fn [i k] {:kind k :id (str k "-" i)}) kinds)))
            (gen/vector gen-kind 0 12)))

(def gen-nonempty-cells
  (gen/such-that seq gen-cells))

(def gen-columns
  "Column counts, 0 included: the grid clamps it to 1."
  (gen/choose 0 5))

(def dirge-keys (vec (sort (keys view/grid-keymap))))

(def move-keys
  (vec (sort (filter #(= :move (verb-of %)) dirge-keys))))

(def gen-key
  "Mostly dirge's own keys, sometimes one only a producer could bind."
  (gen/frequency [[6 (gen/elements dirge-keys)]
                  [1 (gen/elements ["x" "Tab" "o" "r" "u"])]]))

(defn gen-selected
  "What the grid may have selected when `cells` arrive: nothing, one of
   them, or a cell that has since left the grid."
  [cells]
  (gen/one-of (cond-> [(gen/return nil) (gen/return gone-cell)]
                (seq cells) (conj (gen/elements cells)))))

(defn gen-permutation
  [coll]
  (gen/fmap (fn [ks] (mapv second (sort-by first (map vector ks coll))))
            (gen/vector (gen/choose 0 1000000) (count coll))))

(def gen-grid-case
  "An open grid and a key pressed on it."
  (gen/bind gen-cells
            (fn [cells]
              (gen/fmap (fn [[selected columns key]]
                          {:state (opened selected)
                           :event (grid-event cells columns key)})
                        (gen/tuple (gen-selected cells) gen-columns gen-key)))))

(def gen-word
  (gen/one-of [(gen/elements ["" "on" "off" "toggle" "show" "hide" "left"
                              "main" "right" "left|right" "debug" "auto"
                              "next" "prev" "focus" "refresh"])
               gen/string]))

(def gen-command
  (gen/fmap (fn [[name args]] {:type "command" :name name :args args})
            (gen/tuple (gen/elements ["swarm" "panel" "display" "nope"])
                       (gen/vector gen-word 0 3))))

(def gen-producer
  (gen/fmap (fn [[keys usage]]
              {:type "producer"
               :replies [{:name "focus" :target "required"}
                         {:name "unfocus" :target "none"}
                         {:name "refresh" :target "optional"}]
               :keys keys
               :usage usage})
            (gen/tuple (gen/vector (gen/one-of
                                    [(gen/fmap (fn [k] {:key k :verb "refresh"})
                                               (gen/elements ["r" "x" "Enter"]))
                                     (gen/fmap (fn [k] {:key k :verb "focus"})
                                               (gen/elements ["Enter" "f"]))
                                     (gen/fmap (fn [k] {:key k :invoke true :verb "open"})
                                               (gen/elements ["o" "Enter"]))])
                                   0 3)
                       gen/string)))

(def gen-event
  "Any event the view may be sent, unknown kinds included."
  (gen/one-of [(gen/fmap (fn [[cells cols key]] (grid-event cells cols key))
                         (gen/tuple gen-cells gen-columns gen-key))
               gen-command
               gen-producer
               (gen/return {:type "init"})
               (gen/fmap (fn [t] {:type t}) (gen/elements ["bogus" ""]))]))

;; ---------------------------------------------------------------------------
;; Totality: step answers every event with [state effects]

(defn well-formed-step?
  [[state' effects]]
  (and (map? state')
       (vector? effects)
       (every? #(keyword? (:op %)) effects)))

(defprop-total step-is-total-from-closed
  #(view/step closed %)
  gen-event
  {:num-tests 300 :pred well-formed-step?})

(defprop-total step-is-total-on-an-open-grid
  (fn [{:keys [state event]}] (view/step state event))
  gen-grid-case
  {:num-tests 300 :pred well-formed-step?})

(defprop-invariant any-event-sequence-keeps-the-state-shape
  (gen/fmap (fn [events] [closed events]) (gen/vector gen-event 0 25))
  (fn [state event] (first (view/step state event)))
  (fn [state]
    (and (= #{:swarm :panels :producer} (set (keys state)))
         (or (nil? (:swarm state)) (= #{:selected} (set (keys (:swarm state))))))))

;; ---------------------------------------------------------------------------
;; Grid moves

(defspec a-move-always-lands-on-a-cell 300
  (prop/for-all [{:keys [state event]} gen-grid-case
                 key (gen/elements move-keys)]
    (let [event     (assoc event :key key)
          [state' _] (view/step state event)
          cells     (:cells event)]
      (if (empty? cells)
        (= state state')
        (some? (index-in cells (selected-of state')))))))

(defspec left-and-right-move-at-most-one-cell 300
  (prop/for-all [{:keys [state event]} gen-grid-case
                 key (gen/elements ["Left" "h" "Right" "l"])]
    (let [cells (:cells event)
          [state' _] (view/step state (assoc event :key key))]
      (or (empty? cells)
          (let [from (cursor cells (selected-of state))
                to   (index-in cells (selected-of state'))]
            (<= (distance to from) 1))))))

(defspec up-and-down-move-a-whole-row-or-stay 300
  (prop/for-all [{:keys [state event]} gen-grid-case
                 key (gen/elements ["Up" "k" "Down" "j"])]
    (let [cells (:cells event)
          cols  (max 1 (:columns event))
          [state' _] (view/step state (assoc event :key key))]
      (or (empty? cells)
          (let [from (cursor cells (selected-of state))
                to   (index-in cells (selected-of state'))
                d    (distance to from)]
            ;; Up clamps to the first cell when it would leave the grid.
            (or (zero? d) (= d cols) (and (#{"Up" "k"} key) (zero? to))))))))

(defspec home-and-end-reach-the-ends 200
  (prop/for-all [{:keys [state event]} gen-grid-case]
    (let [cells (:cells event)
          land  #(selected-of (first (view/step state (assoc event :key %))))]
      (or (empty? cells)
          (and (= (first cells) (land "Home"))
               (= (peek cells) (land "End")))))))

(defspec a-digit-selects-that-cell-or-nothing-changes 300
  (prop/for-all [{:keys [state event]} gen-grid-case
                 n (gen/choose 1 9)]
    (let [cells (:cells event)
          [state' effects] (view/step state (assoc event :key (str n)))]
      (and (= [] effects)
           (if (<= n (count cells))
             (= (nth cells (dec n)) (selected-of state'))
             (= state state'))))))

;; ---------------------------------------------------------------------------
;; Selection is kept by cell, not by position

(defspec selection-follows-its-cell-through-a-reorder 300
  (prop/for-all [[cells shuffled pick] (gen/bind gen-nonempty-cells
                                                 (fn [cells]
                                                   (gen/tuple (gen/return cells)
                                                              (gen-permutation cells)
                                                              (gen/choose 0 (dec (count cells))))))
                 columns gen-columns]
    (let [chosen (nth cells pick)
          state  (opened chosen)
          [s' _] (view/step state (grid-event shuffled columns "Right"))
          from   (index-in shuffled chosen)
          to     (index-in shuffled (selected-of s'))]
      ;; Right moves from the cell's NEW position, not its old index.
      (= to (min (inc from) (dec (count shuffled)))))))

(defspec a-removed-selection-falls-back-to-the-first-cell 200
  (prop/for-all [cells gen-nonempty-cells
                 columns gen-columns]
    (let [[s' _] (view/step (opened gone-cell) (grid-event cells columns "Right"))]
      (= (nth cells (min 1 (dec (count cells)))) (selected-of s')))))

(defspec events-other-than-grid-and-command-leave-the-selection 300
  (prop/for-all [{:keys [state]} gen-grid-case
                 event (gen/one-of [gen-producer (gen/return {:type "init"})])]
    (= (:swarm state) (:swarm (first (view/step state event))))))

;; ---------------------------------------------------------------------------
;; Leaving the grid

(defspec opening-or-messaging-a-subagent-closes-the-grid 300
  (prop/for-all [cells (gen/such-that #(some (fn [c] (= "agent" (:kind c))) %)
                                      gen-nonempty-cells)
                 columns gen-columns
                 key (gen/elements ["Enter" "m"])]
    (let [agent (first (filter #(= "agent" (:kind %)) cells))
          [s' effects] (view/step (opened agent) (grid-event cells columns key))]
      (and (not (grid-open? s'))
           (= [{:op (if (= key "Enter") :open-agent :message-agent)
                :id (:id agent)}]
              effects)))))

(defspec a-panel-cell-keeps-the-grid-open-on-enter 300
  (prop/for-all [cells (gen/such-that #(some (fn [c] (= "panel" (:kind c))) %)
                                      gen-nonempty-cells)
                 columns gen-columns
                 key (gen/elements ["Enter" "m"])]
    (let [panel (first (filter #(= "panel" (:kind %)) cells))
          [s' _] (view/step (opened panel) (grid-event cells columns key))]
      (grid-open? s'))))

(defspec every-grid-key-is-a-no-op-while-the-grid-is-closed 300
  (prop/for-all [cells gen-cells
                 columns gen-columns
                 key gen-key]
    (= [closed []] (view/step closed (grid-event cells columns key)))))

;; ---------------------------------------------------------------------------
;; /swarm

(defn run-command
  [state name & args]
  (first (view/step state {:type "command" :name name :args (vec args)})))

(deftest swarm-toggle-flips-the-grid-and-on-off-are-idempotent
  (doseq [start [closed (opened nil)]]
    (testing (str "from " (if (grid-open? start) "open" "closed"))
      (is (= (grid-open? start) (grid-open? (run-command (run-command start "swarm") "swarm"))))
      (is (not= (grid-open? start) (grid-open? (run-command start "swarm"))))
      (is (grid-open? (run-command (run-command start "swarm" "on") "swarm" "on")))
      (is (not (grid-open? (run-command (run-command start "swarm" "off") "swarm" "off")))))))

;; ---------------------------------------------------------------------------
;; The model's key and command lists are the registries

(deftest view-commands-are-the-command-registry
  (is (= (vec (sort (keys view/commands)))
         (:view_commands (view/model closed)))))

(defspec grid-keys-are-dirge-keys-plus-the-producers 200
  (prop/for-all [producer gen-producer]
    (let [state (first (view/step closed producer))]
      (= (vec (sort (distinct (concat (keys view/grid-keymap)
                                      (map :key (:keys producer))))))
         (:grid_keys (view/model state))))))
