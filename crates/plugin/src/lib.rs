//! argent plugin layer: a stable C ABI for out-of-tree tools, and the MCP
//! stdio client that wires external servers into the registry.

pub mod abi;
pub mod loader;
pub mod mcp;

pub use abi::{
    LoadedPlugin, ArgentPluginV1, PluginToolDescV1, PluginToolInfo, ABI_VERSION, ENTRY_SYMBOL,
};
pub use loader::{is_plugin_file, load_all, plugin_directories, PluginToolAdapter};
pub use mcp::{discover as mcp_discover, parse_specs as mcp_parse_specs, PLUGIN as MCP_PLUGIN};
