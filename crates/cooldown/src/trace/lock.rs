use std::fs::{File, TryLockError};

#[cfg(not(windows))]
pub(super) fn try_lock(file: &File) -> Result<(), TryLockError> {
    file.try_lock()
}

#[cfg(windows)]
pub(super) fn try_lock(file: &File) -> Result<(), TryLockError> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

    // Windows locks deny payload reads too, so reserve one byte at i64::MAX - 1 beyond the trace data.
    // Closing the owning file releases the lease without a sidecar or filename-dependent identity.
    let mut overlapped = OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: 0xffff_fffe,
                OffsetHigh: 0x7fff_ffff,
            },
        },
        hEvent: std::ptr::null_mut(),
    };
    // SAFETY: both callers own synchronous file handles and `overlapped` lives through the call.
    // FAIL_IMMEDIATELY prevents a pending wait; the stack value owns no event or other resource.
    #[expect(
        unsafe_code,
        reason = "LockFileEx requires a live file handle and initialized OVERLAPPED"
    )]
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle().cast(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &raw mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error().and_then(|code| u32::try_from(code).ok()) == Some(ERROR_LOCK_VIOLATION) {
        Err(TryLockError::WouldBlock)
    } else {
        Err(TryLockError::Error(err))
    }
}
