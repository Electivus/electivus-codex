use std::io;
use std::path::Path;

const SQLITE_RESERVED_BYTE: u64 = 0x4000_0001;
const SQLITE_WAL_WRITE_LOCK_BYTE: u64 = 120;

pub(super) fn writer_is_active(database_path: &Path) -> io::Result<bool> {
    if byte_has_exclusive_lock(database_path, SQLITE_RESERVED_BYTE)? {
        return Ok(true);
    }
    let mut shared_memory_path = database_path.as_os_str().to_os_string();
    shared_memory_path.push("-shm");
    let shared_memory_path = Path::new(&shared_memory_path);
    if !shared_memory_path.exists() {
        return Ok(false);
    }
    byte_has_exclusive_lock(shared_memory_path, SQLITE_WAL_WRITE_LOCK_BYTE)
}

#[cfg(unix)]
fn byte_has_exclusive_lock(path: &Path, offset: u64) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    let file = std::fs::File::open(path)?;
    let offset = libc::off_t::try_from(offset)
        .map_err(|_| io::Error::other("SQLite lock offset is not addressable"))?;
    let mut lock = libc::flock {
        l_type: libc::F_RDLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: offset,
        l_len: 1,
        l_pid: 0,
    };
    // SAFETY: `lock` points to an initialized `flock` for this open file descriptor.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
    if result == 0 {
        lock.l_type = libc::F_UNLCK as libc::c_short;
        // SAFETY: this releases the byte-range lock acquired immediately above.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == -1 {
            return Err(io::Error::last_os_error());
        }
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(code) if code == libc::EACCES || code == libc::EAGAIN) {
        Ok(true)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn byte_has_exclusive_lock(path: &Path, offset: u64) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_FAIL_IMMEDIATELY;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let file = std::fs::File::open(path)?;
    let mut overlapped = OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        Anonymous: windows_sys::Win32::System::IO::OVERLAPPED_0 {
            Anonymous: windows_sys::Win32::System::IO::OVERLAPPED_0_0 {
                Offset: offset as u32,
                OffsetHigh: (offset >> 32) as u32,
            },
        },
        hEvent: 0,
    };
    // SAFETY: the handle and `OVERLAPPED` remain valid until the matching unlock below.
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        // SAFETY: this releases the byte-range lock acquired immediately above.
        if unsafe { UnlockFileEx(file.as_raw_handle() as _, 0, 1, 0, &mut overlapped) } == 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(true)
    } else {
        Err(error)
    }
}
