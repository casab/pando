//! Which boot this is: an id that changes whenever pids start again.

/// An identifier of this boot of the machine: the same for every process
/// until it restarts, and different after. `None` where the system does
/// not say. Read once.
pub fn id() -> Option<&'static str> {
    static BOOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BOOT.get_or_init(imp::read).as_deref()
}

/// Whether something written down during boot `written` was written
/// during this one, `now`.
pub fn same(written: &str, now: &str) -> bool {
    written == now
}

#[cfg(target_os = "macos")]
mod imp {
    /// `kern.bootsessionuuid`: a new one at every boot.
    pub(super) fn read() -> Option<String> {
        let name = c"kern.bootsessionuuid";
        let mut len: libc::size_t = 0;
        // SAFETY: a null buffer asks for the length only.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` is `len` bytes long, which is what the kernel is told.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        buf.truncate(len);
        let id = String::from_utf8_lossy(&buf)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        (!id.is_empty()).then_some(id)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    /// The kernel's boot id.
    pub(super) fn read() -> Option<String> {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let id = id.trim().to_string();
        (!id.is_empty()).then_some(id)
    }
}
