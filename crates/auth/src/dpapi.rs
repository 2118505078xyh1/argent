//! Windows DPAPI wrapper for the credential file.
//!
//! The auth file is encrypted with the current Windows user's DPAPI key, so a
//! copied file is useless on another account or machine. On Unix the file is
//! left in plaintext and protected by mode `0600`; a keyring backend can replace
//! this without changing the `AuthStore` API.

/// Marks a file this module encrypted.
pub const HEADER: &[u8] = b"ARGENT-DPAPI-1\n";

#[cfg(windows)]
pub fn protect(data: &[u8]) -> argent_core::error::Result<Option<Vec<u8>>> {
    use std::ptr;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const DataBlob,
            description: *const u16,
            entropy: *const DataBlob,
            reserved: *mut core::ffi::c_void,
            prompt: *mut core::ffi::c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(handle: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    const UI_FORBIDDEN: u32 = 0x1;

    let input = DataBlob {
        cb_data: data.len() as u32,
        pb_data: data.as_ptr() as *mut u8,
    };
    let mut output = DataBlob {
        cb_data: 0,
        pb_data: ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
            UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(argent_core::error::Error::io(format!(
            "DPAPI protect failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec() };
    unsafe {
        LocalFree(output.pb_data as *mut core::ffi::c_void);
    }
    Ok(Some(bytes))
}

#[cfg(windows)]
pub fn unprotect(data: &[u8]) -> argent_core::error::Result<Vec<u8>> {
    use std::ptr;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptUnprotectData(
            data_in: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut core::ffi::c_void,
            prompt: *mut core::ffi::c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(handle: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    const UI_FORBIDDEN: u32 = 0x1;

    let input = DataBlob {
        cb_data: data.len() as u32,
        pb_data: data.as_ptr() as *mut u8,
    };
    let mut output = DataBlob {
        cb_data: 0,
        pb_data: ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
            UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(argent_core::error::Error::io(format!(
            "DPAPI unprotect failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec() };
    unsafe {
        LocalFree(output.pb_data as *mut core::ffi::c_void);
    }
    Ok(bytes)
}

#[cfg(not(windows))]
pub fn protect(_data: &[u8]) -> argent_core::error::Result<Option<Vec<u8>>> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn unprotect(data: &[u8]) -> argent_core::error::Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_dpapi() {
        let plain = b"{\"providers\":[]}";
        let protected = protect(plain).expect("protect").expect("encrypted");
        assert_ne!(protected, plain);
        let restored = unprotect(&protected).expect("unprotect");
        assert_eq!(restored, plain);
    }
}
