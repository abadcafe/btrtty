use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

const DEFAULT_LOCK_PATH: &str = "/tmp/dev.btrtty.terminal.lock";

/// Process-lifetime exclusive ownership of the btrtty service.
pub struct InstanceLock {
    file: File,
}

impl InstanceLock {
    /// Acquire the process-wide btrtty service lock.
    ///
    /// # Errors
    ///
    /// Returns an error if another btrtty process owns the Bluetooth service.
    pub fn acquire_default() -> Result<Self> {
        Self::acquire(Path::new(DEFAULT_LOCK_PATH))
    }

    /// Acquire the exclusive lock represented by `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if another process owns the lock or the lock file
    /// cannot be opened or locked safely.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("open btrtty instance lock {}", path.display()))?;

        // SAFETY: `file` owns a valid open descriptor for this call and for the
        // entire lifetime of the returned guard.
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            let code = error.raw_os_error();
            if code == Some(libc::EWOULDBLOCK) || code == Some(libc::EAGAIN) {
                bail!("another btrtty process is already running");
            }
            return Err(error).context("lock btrtty instance");
        }

        Ok(Self { file })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // SAFETY: The descriptor remains valid until `file` is dropped after
        // this method returns. Closing also releases the lock if unlock fails.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
