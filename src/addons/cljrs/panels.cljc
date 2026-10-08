(ns dirge.panels
  "The external panels' policy as a pure reducer, run inside the view
   engine (`dirge.view`). A panel feed's ops arrive undecoded; `step`
   folds one into the panel state and names the effects. Core dirge only
   sanitises, bounds and paints what a `paint` effect carries.

   A row is a vector of runs {:text :face}; a face is a wire name
   (`added`, `warn`, `dim`, ...) that core maps onto its palette.

   What the feed can say is registered as data in `ops` (op name ->
   handler), so a new op is one more entry."
  (:require [clojure.string :as str]))

;; SPDX-License-Identifier: GPL-3.0-only

(def max-panels
  "Panels kept at once; a new one beyond this evicts the oldest."
  16)

(def max-rows
  "Rows kept per panel. A shown body keeps its first rows; an
   accumulating panel drops its oldest."
  1000)

(def empty-state
  {:panels {} :order [] :focused nil})

;; ---------------------------------------------------------------------------
;; Effects

(defn unpaint
  [id]
  {:op :unpaint :id id})

(defn span
  [text face]
  {:text (str text) :face (or face "")})

(defn paint
  "The effect that sets panel `id` as the state holds it."
  [state id]
  (let [p (get-in state [:panels id])]
    {:op     :paint
     :id     id
     :title  (:title p)
     :rows   (if (and (= id (:focused state)) (:cursor? p) (seq (:rows p)))
               (assoc (:rows p) (:cursor p)
                      (into [(span "▸ " "cursor")]
                            (mapv #(assoc % :face "cursor") (nth (:rows p) (:cursor p)))))
               (:rows p))
     :tail   (boolean (:tail p))
     :offset (:offset p)
     :focus  (= id (:focused state))}))

;; ---------------------------------------------------------------------------
;; Rows

(defn line->row
  "A wire line as a row: a bare string, {:text :face}, or {:face :spans}
   whose runs (strings, or {:text :face}) replace :text."
  [line]
  (cond
    (string? line) [(span line "")]
    (map? line)
    (let [face  (or (:face line) "")
          spans (:spans line)]
      (if (seq spans)
        (mapv (fn [s]
                (if (string? s)
                  (span s face)
                  (span (or (:text s) "") (or (:face s) face))))
              spans)
        [(span (or (:text line) "") face)]))
    :else nil))

(defn lines-of
  "`text` cut at newlines, keeping a trailing empty line."
  [text]
  (let [parts (vec (str/split text #"\n"))
        parts (if (empty? parts) [""] parts)
        parts (if (and (str/ends-with? text "\n") (not= text "\n"))
                (conj parts "")
                parts)]
    (mapv #(str/replace % #"\r$" "") parts)))

(defn conj-span
  [row text face]
  (if (= "" text) row (conj row (span text face))))

(defn split-row
  "A row cut at every newline inside its runs; each run keeps its face."
  [row]
  (let [step (fn [[cur out] {:keys [text face]}]
               (let [parts (lines-of text)]
                 (reduce (fn [[c o] part] [(conj-span [] part face) (conj o c)])
                         [(conj-span cur (first parts) face) out]
                         (rest parts))))
        [cur out] (reduce step [[] []] row)]
    (conj out cur)))

(defn row-entries-of
  "Body rows with parallel id and payload; only the first split row inherits metadata."
  [op]
  (let [lines (if (sequential? (:lines op)) (:lines op)
                  (when (string? (:text op)) [(:text op)]))]
    (vec (mapcat (fn [line]
                   (when-let [row (line->row line)]
                     (map-indexed (fn [i split]
                                    {:row split
                                     :id (when (and (zero? i) (map? line)) (:id line))
                                     :payload (when (and (zero? i) (map? line)) (:payload line))})
                                  (split-row row))))
                 lines))))

(defn rows-of [op] (mapv :row (row-entries-of op)))

(defn row-ids-of
  "Stable identifiers parallel to the rows (only the first split row gets an id)."
  [op]
  (mapv :id (row-entries-of op)))

(defn row-payloads-of [op] (mapv :payload (row-entries-of op)))

(defn row-text
  [row]
  (apply str (map :text row)))

(def title-faces
  "Faces a producer renders its document title with."
  #{"title" "heading" "accent" "info" "highlight" "link" "hunk"})

(defn drop-title-row
  "A producer that renders its document title as the first row would
   repeat the box title: drop that row, and a blank row after it."
  [rows title]
  (let [r (first rows)]
    (if (and (seq r)
             (= title (row-text r))
             (every? #(contains? title-faces (str/lower-case (str/trim (:face %)))) r))
      (let [more (vec (rest rows))]
        (if (and (seq more) (= "" (row-text (first more))))
          (vec (rest more))
          more))
      rows)))

;; ---------------------------------------------------------------------------
;; State

(defn panel-id
  [op]
  (let [id (:id op)]
    (when (and (string? id) (not (str/blank? id)))
      id)))

(defn title-of
  [op id]
  (or (:title op) id))

(defn clamp-offset
  [p]
  (assoc p :offset (min (or (:offset p) 0) (max 0 (dec (count (:rows p)))))
           :cursor (min (or (:cursor p) 0) (max 0 (dec (count (:rows p)))))))

(defn drop-panel
  [state id]
  (-> state
      (update :panels dissoc id)
      (update :order (fn [o] (vec (remove #(= % id) o))))
      (update :focused #(if (= % id) nil %))))

(defn ensure-panel
  "[state' effects]: panel `id` exists, evicting the oldest when full."
  [state id title tail]
  (if (contains? (:panels state) id)
    [state []]
    (let [full?   (>= (count (:order state)) max-panels)
          old     (first (:order state))
          state   (if full? (drop-panel state old) state)
          evicted (if full? [(unpaint old)] [])]
      [(-> state
           (assoc-in [:panels id] {:title title :rows [] :tail tail :offset 0 :cursor 0 :cursor? false :keys {} :row-ids [] :row-payloads [] :payload {}})
           (update :order conj id))
       evicted])))

;; ---------------------------------------------------------------------------
;; Ops

(def chord-names
  "Wire chords to the names emitted by promote::key_name. Unsupported
   modified chords cannot be claimed by the panel route."
  {"enter" "Enter" "return" "Enter" "tab" "Tab"
   "shift-tab" "BackTab" "backtab" "BackTab"
   "esc" "Esc" "escape" "Esc"
   "pgdn" "PgDn" "pagedown" "PgDn" "pagedn" "PgDn"
   "pgup" "PgUp" "pageup" "PgUp"
   "up" "Up" "down" "Down" "left" "Left" "right" "Right"
   "home" "Home" "end" "End"})

(defn normalize-panel-keys
  "Pure conversion from capabilities chords and invoke declarations to
   routable names and verbs. Bad entries are omitted."
  [keys]
  (into {}
        (keep (fn [[chord declaration]]
                (let [spelling (when (or (string? chord) (keyword? chord)) (str/trim (name chord)))
                      name (when spelling
                             (or (get chord-names (str/lower-case spelling))
                                 (when (= 1 (count spelling)) spelling)))
                      verb (if (string? declaration) declaration
                               (when (map? declaration) (or (:invoke declaration) (get declaration "invoke"))))]
                  (when (and name (string? verb) (not (str/blank? verb)))
                    [name verb]))))
        (if (map? keys) keys {})))

(defn show-panel
  [state op]
  (if-let [id (panel-id op)]
    (let [title       (title-of op id)
          [state evs] (ensure-panel state id title false)
          entries     (row-entries-of op)
          all-rows    (mapv :row entries)
          all-ids     (mapv :id entries)
          all-payloads (mapv :payload entries)
          dropped     (- (count all-rows) (count (drop-title-row all-rows title)))
          rows        (vec (take max-rows (drop dropped all-rows)))
          row-ids     (vec (take max-rows (drop dropped all-ids)))
          row-payloads (vec (take max-rows (drop dropped all-payloads)))
          keys        (normalize-panel-keys (:keys op))
          malformed   (when (map? (:keys op))
                        (remove (fn [[chord declaration]]
                                  (let [single (normalize-panel-keys {chord declaration})]
                                    (seq single)))
                                (:keys op)))
          state       (update-in state [:panels id]
                                 #(clamp-offset (assoc % :title title :rows rows :row-ids row-ids :row-payloads row-payloads
                                                         :payload (if (map? (:payload op)) (:payload op) {})
                                                         :keys keys
                                                         :cursor? (true? (:cursor op)) :tail false)))
          state       (if (or (true? (:cursor op)) (seq keys))
                        (assoc state :focused id) state)]
      [state (cond-> (conj evs (paint state id))
               (seq malformed) (conj {:op :notify :level :warn
                                      :text "Ignoring malformed panel key declaration"}))])
    [state []]))

(defn close-panel
  [state op]
  (if-let [id (panel-id op)]
    [(drop-panel state id) [(unpaint id)]]
    [state []]))

(defn focus-tab
  [state op]
  (if-let [id (panel-id op)]
    (let [title       (or (:title op) id)
          [state evs] (ensure-panel state id title true)
          state       (-> state
                          (update-in [:panels id] assoc :title title :tail true)
                          (assoc :focused id))]
      [state (conj evs (paint state id))])
    [state []]))

(defn append-tab
  [state op]
  (let [id   (panel-id op)
        line (or (:line op)
                 (when (string? (:text op)) {:text (:text op) :face (:face op)}))
        row  (when line (line->row line))]
    (if (and id row)
      (let [new         (split-row row)
            [state evs] (ensure-panel state id id true)
            state       (update-in state [:panels id]
                                   (fn [p]
                                     (let [rows (into (:rows p) new)
                                           over (- (count rows) max-rows)]
                                       (clamp-offset
                                        (assoc p
                                               :rows (if (pos? over) (subvec rows over) rows)
                                               :row-ids (vec (take max-rows (drop (max over 0)
                                                                                  (concat (:row-ids p) (repeat (count new) nil)))))
                                               :row-payloads (vec (take max-rows (drop (max over 0)
                                                                                       (concat (:row-payloads p) (repeat (count new) nil)))))
                                               ;; A reader scrolled back keeps their place.
                                               :offset (if (pos? (:offset p))
                                                         (+ (:offset p) (count new))
                                                         0))))))]
        [state (conj evs (paint state id))])
      [state []])))

(def scroll-keys {"j" 1 "Down" 1 "k" -1 "Up" -1
                  "PgDn" 10 "PgUp" -10})

(defn panel-keys [state]
  (if-let [p (get-in state [:panels (:focused state)])]
    (vec (sort (distinct (concat ["Esc"] (map name (keys (:keys p)))
                                (keys scroll-keys)))))
    []))

(defn key-step
  "Only the focused panel receives its declared keys. Cursor is a row index;
   offset counts back from the top, and follows it as it moves."
  [state {:keys [key panel]}]
  (let [p (get-in state [:panels panel])
        verb (or (get (:keys p) key) (get (:keys p) (keyword key)))
        delta (get scroll-keys key)]
    (cond
      (not= panel (:focused state)) [state []]
      (= key "Esc") (let [state (assoc state :focused nil)]
                      [state [(paint state panel)]])
      (and (string? verb) (not (str/blank? verb)))
      [state [{:op :reply :action "invoke"
               :payload {:panel panel :verb verb
                         :row (get (:row-ids p) (:cursor p))
                         :payload (or (get (:row-payloads p) (:cursor p)) (:payload p) {})}}]]
      delta (let [state (update-in state [:panels panel]
                                   (fn [p] (let [cursor (max 0 (min (max 0 (dec (count (:rows p))))
                                                                        (+ (:cursor p) delta)))]
                                             (assoc p :cursor cursor :offset cursor))))]
              [state [(paint state panel)]])
      :else [state []])))

(def levels
  {"warn" :warn "warning" :warn "error" :error "err" :error})

(defn notify
  [state op]
  (let [msg (or (:message op) (:text op))]
    (if (string? msg)
      [state [{:op    :notify
               :level (get levels (some-> (:level op) str str/trim str/lower-case) :info)
               :text  msg}]]
      [state []])))

(defn feed-ended
  "The producer went away: close every panel it opened. A panel an
   addon opened (`:owner \"addon\"`, see `addons::sink`) shares the
   panes but not the producer's lifetime, so it stays."
  [state _]
  (let [state (or state empty-state)
        gone  (vec (remove #(= "addon" (get-in state [:panels % :owner]))
                           (:order state)))]
    [(reduce drop-panel state gone) (mapv unpaint gone)]))

(defn open-file [state op]
  (if (and (string? (:path op)) (not (str/blank? (:path op))))
    [state [{:op :open-file :path (:path op) :line (:line op) :diff (:diff op)}]]
    [state []]))

(def ops
  "Feed op name -> (fn [state op] [state' effects]). An op not listed
   is ignored: a newer producer never breaks the view. Names are the
   neutral ones; core renames a producer's dialect before it gets here."
  {"show"       show-panel
   "close"      close-panel
   "focus"      focus-tab
   "append"     append-tab
   "notify"     notify
   "open-file"  open-file
   "feed/ended" feed-ended})

(defn step
  "[state' effects] for one feed `op`. An op that names an `:owner`
   (an addon's, see `addons::sink`) marks the panel it touched with it,
   so `feed-ended` can tell the producer's panels from the addon's."
  [state op]
  (if-let [f (get ops (:op op))]
    (let [[s effects] (f (or state empty-state) op)
          id          (panel-id op)
          owner       (:owner op)]
      [(if (and (string? owner) id (contains? (:panels s) id))
         (assoc-in s [:panels id :owner] owner)
         s)
       effects])
    [state []]))
