//! The plugin ABI: a stable C contract for out-of-tree tools.
//!
//! A plugin is a dynamic library exporting one symbol:
//!
//! ```c
//! const ArgentPluginV1 *argent_plugin_v1(void);
//! ```
//!
//! Everything crosses the boundary as C types. Strings are NUL-terminated
//! UTF-8 allocated by the plugin and released through the plugin's own
//! `free_string`, so the two sides never need to share an allocator.

use std::ffi::{c_char, c_void, CStr, CString};

/// The ABI revision this build speaks.
pub const ABI_VERSION: u32 = 1;

/// The symbol a plugin must export.
pub const ENTRY_SYMBOL: &str = "argent_plugin_v1";

/// One tool's metadata, as returned by the plugin.
#[repr(C)]
pub struct PluginToolDescV1 {
    /// Tool name, NUL-terminated UTF-8.
    pub name: *const c_char,
    /// One-line description.
    pub description: *const c_char,
    /// JSON Schema for the arguments, as a JSON string.
    pub parameters_json: *const c_char,
}

/// The vtable a plugin returns.
///
/// The struct is only ever extended by appending fields; `abi_version` lets
/// the host reject a plugin it cannot speak to.
#[repr(C)]
pub struct ArgentPluginV1 {
    /// Must equal [`ABI_VERSION`].
    pub abi_version: u32,
    /// Plugin name, NUL-terminated UTF-8.
    pub name: *const c_char,
    /// Plugin version string.
    pub version: *const c_char,
    /// Number of tools this plugin provides.
    pub tool_count: extern "C" fn() -> u32,
    /// Describe tool `index`. Returns null for an out-of-range index.
    pub tool_desc: extern "C" fn(index: u32) -> PluginToolDescV1,
    /// Run tool `index` with a JSON argument string.
    ///
    /// Returns a JSON string of the form
    /// `{"content":"...","isError":false}`, or null on a fatal failure. The
    /// caller must release the result with `free_string`.
    pub tool_call: extern "C" fn(index: u32, args_json: *const c_char) -> *const c_char,
    /// Release a string the plugin allocated.
    pub free_string: extern "C" fn(ptr: *const c_char),
}

/// A plugin loaded into the process.
pub struct LoadedPlugin {
    /// Keeps the library mapped.
    _library: Library,
    /// The vtable the plugin returned.
    vtable: *const ArgentPluginV1,
    /// The plugin's name.
    pub name: String,
    /// The plugin's version.
    pub version: String,
}

// The vtable is immutable after load and the plugin is expected to be
// thread-safe; the host serialises calls through the tool registry.
unsafe impl Send for LoadedPlugin {}
unsafe impl Sync for LoadedPlugin {}

/// One tool described by a plugin.
#[derive(Debug, Clone)]
pub struct PluginToolInfo {
    /// Index within the plugin's tool table.
    pub index: u32,
    pub name: String,
    pub description: String,
    /// JSON Schema, as text.
    pub parameters_json: String,
}

impl LoadedPlugin {
    /// Load a plugin from a shared library path.
    pub fn load(path: &std::path::Path) -> Result<LoadedPlugin, String> {
        let library = Library::open(path)?;
        let symbol = library.symbol(ENTRY_SYMBOL)?;

        // SAFETY: the symbol is declared by the plugin to have this signature.
        // A plugin that lies about it is outside what any host can defend
        // against, so the version field is the real guard.
        let entry: extern "C" fn() -> *const ArgentPluginV1 = unsafe {
            std::mem::transmute::<*mut c_void, extern "C" fn() -> *const ArgentPluginV1>(symbol)
        };
        let vtable = entry();
        if vtable.is_null() {
            return Err(format!("{} returned a null plugin table", path.display()));
        }

        let version = unsafe { (*vtable).abi_version };
        if version != ABI_VERSION {
            return Err(format!(
                "{} speaks ABI version {} but this host speaks {}",
                path.display(),
                version,
                ABI_VERSION
            ));
        }

        let name =
            unsafe { cstr_to_string((*vtable).name) }.unwrap_or_else(|| "unnamed".to_string());
        let plugin_version =
            unsafe { cstr_to_string((*vtable).version) }.unwrap_or_else(|| "0.0.0".to_string());

        Ok(LoadedPlugin {
            _library: library,
            vtable,
            name,
            version: plugin_version,
        })
    }

    /// Every tool the plugin provides.
    pub fn tools(&self) -> Vec<PluginToolInfo> {
        let mut out = Vec::new();
        // SAFETY: the vtable was validated on load.
        unsafe {
            let count = ((*self.vtable).tool_count)();
            for index in 0..count {
                let desc = ((*self.vtable).tool_desc)(index);
                let name = match cstr_to_string(desc.name) {
                    Some(name) if !name.is_empty() => name,
                    _ => continue,
                };
                out.push(PluginToolInfo {
                    index,
                    name,
                    description: cstr_to_string(desc.description).unwrap_or_default(),
                    parameters_json: cstr_to_string(desc.parameters_json)
                        .unwrap_or_else(|| r#"{"type":"object","properties":{}}"#.to_string()),
                });
            }
        }
        out
    }

    /// Invoke a tool by index.
    pub fn call(&self, index: u32, args_json: &str) -> Result<String, String> {
        let args = CString::new(args_json).map_err(|_| "arguments contain a NUL byte")?;
        // SAFETY: the vtable was validated on load and the argument is a valid
        // C string for the duration of the call.
        unsafe {
            let result = ((*self.vtable).tool_call)(index, args.as_ptr());
            if result.is_null() {
                return Err(format!(
                    "plugin '{}' tool {} returned no result",
                    self.name, index
                ));
            }
            let text = cstr_to_string(result).unwrap_or_default();
            // The plugin owns the string; release it through its own allocator.
            ((*self.vtable).free_string)(result);
            Ok(text)
        }
    }
}

/// Convert a plugin-owned C string into an owned Rust string.
unsafe fn cstr_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
}

/// A dynamically loaded library.
struct Library {
    handle: *mut c_void,
}

impl Library {
    /// Open a shared library.
    fn open(path: &std::path::Path) -> Result<Library, String> {
        platform::open(path)
            .map(|handle| Library { handle })
            .ok_or_else(|| format!("could not load {}", path.display()))
    }

    /// Resolve a symbol.
    fn symbol(&self, name: &str) -> Result<*mut c_void, String> {
        platform::symbol(self.handle, name).ok_or_else(|| {
            format!(
                "library does not export '{}'; is it built as a plugin (cdylib)?",
                name
            )
        })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        platform::close(self.handle);
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    extern "system" {
        fn LoadLibraryW(path: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }

    pub fn open(path: &Path) -> Option<*mut c_void> {
        let mut wide: Vec<u16> = OsStr::new(path).encode_wide().collect();
        wide.push(0);
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        if handle.is_null() {
            None
        } else {
            Some(handle)
        }
    }

    pub fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
        let mut c_name = name.as_bytes().to_vec();
        c_name.push(0);
        let address = unsafe { GetProcAddress(handle, c_name.as_ptr()) };
        if address.is_null() {
            None
        } else {
            Some(address)
        }
    }

    pub fn close(handle: *mut c_void) {
        unsafe {
            FreeLibrary(handle);
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::path::Path;

    const RTLD_NOW: c_int = 2;

    extern "C" {
        fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
    }

    pub fn open(path: &Path) -> Option<*mut c_void> {
        let text = CString::new(path.to_string_lossy().as_bytes()).ok()?;
        let handle = unsafe { dlopen(text.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            None
        } else {
            Some(handle)
        }
    }

    pub fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
        let text = CString::new(name).ok()?;
        let address = unsafe { dlsym(handle, text.as_ptr()) };
        if address.is_null() {
            None
        } else {
            Some(address)
        }
    }

    pub fn close(handle: *mut c_void) {
        unsafe {
            dlclose(handle);
        }
    }
}

/// The plugin-side helper this crate exports for convenience.
///
/// A plugin written in Rust can use this to build its vtable without
/// re-declaring the layout.
pub mod plugin_side {
    use super::*;

    /// Allocate a C string the host can read and later free.
    pub fn leak_string(text: &str) -> *const c_char {
        match CString::new(text) {
            Ok(value) => value.into_raw() as *const c_char,
            Err(_) => std::ptr::null(),
        }
    }

    /// Release a string produced by [`leak_string`].
    pub fn release_string(ptr: *const c_char) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            drop(CString::from_raw(ptr as *mut c_char));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_abi_version() {
        assert_eq!(ABI_VERSION, 1);
        assert_eq!(ENTRY_SYMBOL, "argent_plugin_v1");
    }

    #[test]
    fn loading_a_missing_library_fails_cleanly() {
        let path = std::path::Path::new("/definitely/not/a/library.so");
        match LoadedPlugin::load(path) {
            Ok(_) => panic!("loading a nonexistent library should fail"),
            Err(error) => assert!(error.contains("could not load"), "got: {}", error),
        }
    }

    #[test]
    fn leak_and_release_round_trip() {
        let ptr = plugin_side::leak_string("hello plugin");
        assert!(!ptr.is_null());
        let text = unsafe { cstr_to_string(ptr) }.expect("readable");
        assert_eq!(text, "hello plugin");
        plugin_side::release_string(ptr);
    }
}
