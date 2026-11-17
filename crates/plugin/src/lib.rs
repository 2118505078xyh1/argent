//! argent plugin layer: a stable C ABI for out-of-tree tools.

pub mod abi;
pub mod loader;

pub use abi::{
    ArgentPluginV1, LoadedPlugin, PluginToolDescV1, PluginToolInfo, ABI_VERSION, ENTRY_SYMBOL,
};
pub use loader::{is_plugin_file, load_all, plugin_directories, PluginToolAdapter};
