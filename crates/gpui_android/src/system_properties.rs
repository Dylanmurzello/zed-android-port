//! Android system properties the runtime seeds the guest from.

use std::ffi::{CStr, CString};

/// `__system_property_get`: the value of `name`, `None` when unset or
/// unreadable.
pub fn get(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    // PROP_VALUE_MAX
    let mut value = [0u8; 92];
    // SAFETY: bionic writes at most PROP_VALUE_MAX bytes, NUL included,
    // into the buffer.
    let length = unsafe { libc::__system_property_get(name.as_ptr(), value.as_mut_ptr().cast()) };
    if length <= 0 {
        return None;
    }
    let text = CStr::from_bytes_until_nul(&value).ok()?.to_str().ok()?;
    (!text.is_empty()).then(|| text.to_string())
}

/// The device's IANA time zone (`persist.sys.timezone`).
pub fn timezone() -> Option<String> {
    get("persist.sys.timezone")
}
