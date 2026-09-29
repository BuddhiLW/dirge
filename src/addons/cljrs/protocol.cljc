(ns dirge.addon.protocol
  "dirge's own addon protocol, bound when neither `addons.protocol_ns` nor
   an addon manifest's `:addon/protocol-ns` names another. Built into dirge,
   so an addon needs no library on its classpath to implement it.")

;; SPDX-License-Identifier: GPL-3.0-only

(defprotocol IAddon
  (addon-id [this] "The addon's unique id.")
  (initialize! [this config] "Start the addon with dirge's host config.")
  (shutdown! [this] "Stop the addon and release what it holds.")
  (tools [this] "Tool definitions: [{:name :description :inputSchema :handler}].")
  (hooks [this] "Hook map keyed by `:dirge/*` hook keys.")
  (health [this] "Health data shown by `/addons`."))

(defn addon?
  "True when `x` implements IAddon."
  [x]
  (satisfies? IAddon x))
