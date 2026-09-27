//! Addon tools as agent loop tools.
//!
//! An addon tool is described by a name, a description and a JSON schema
//! for its arguments. Calls are forwarded to the isolate thread as JSON and
//! the JSON reply becomes the tool result.

/// Static description of a tool contributed by an addon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonToolMeta {
    /// Id of the addon that owns the tool.
    pub addon_id: String,
    /// Tool name as the model sees it.
    pub name: String,
    /// Human-readable description shown to the model.
    pub description: String,
    /// JSON schema of the arguments, as a JSON string.
    pub input_schema: String,
}
