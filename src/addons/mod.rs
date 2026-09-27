//! Clojure addon host (cargo feature `addons`).
//!
//! An addon is a Clojure namespace (`.cljc` or `.cljrs`) that dirge loads
//! into an embedded clojurust interpreter. Each addon ships a small EDN
//! manifest naming its id, the namespace to load and the constructor to
//! call; the constructor returns an addon instance that may contribute
//! tools, hooks and slash commands.
//!
//! Layout:
//! - [`manifest`]: parse and validate the addon manifest EDN.
//! - [`isolate`]: the dedicated interpreter thread that owns every
//!   clojurust value; the rest of dirge talks to it with JSON strings only.
//! - [`harness`]: the `dirge.harness` namespace exposed to addon code.
//! - [`tool`]: adapters turning addon tools into agent loop tools.
//!
//! This module is a skeleton: only manifest parsing is implemented so far.

// The host is being built up incrementally; most items are not wired into
// the rest of dirge yet.
#![allow(dead_code)]

pub mod harness;
pub mod isolate;
pub mod manifest;
pub mod tool;
