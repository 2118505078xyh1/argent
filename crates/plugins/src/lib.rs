//! ZCode-compatible plugin support for argent.

pub mod manager;
pub mod manifest;
pub mod marketplace;
pub mod runtime;
pub mod signature;

pub use manager::{DiscoveredPlugin, InstallPolicy, PluginManager};
pub use manifest::{
    find_manifest, McpServerDecl, PluginManifest, ScriptDecl, UiSurface, UserConfigSpec,
    MANIFEST_PATHS,
};
pub use marketplace::{Marketplace, MarketplaceEntry, MarketplaceRecord, MarketplaceStore};
#[cfg(feature = "script-plugins")]
pub use runtime::load_script_plugins;
pub use runtime::{
    add_mcp_servers, component, expand_plugin_text, load_trusted_hooks, workspace_hook_files,
};
pub use signature::{SignatureMode, SignaturePolicy, SignatureStatus};
