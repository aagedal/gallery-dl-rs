//! Upstream `--write-log FILE`: copy everything written to standard error,
//! including child processes' diagnostics, to FILE.
use crate::{Result, error};
use std::{path::Path, sync::Mutex};

struct Tee {
    #[cfg(unix)]
    saved: libc::c_int,
    thread: std::thread::JoinHandle<()>,
}

static TEE: Mutex<Option<Tee>> = Mutex::new(None);

/// Start copying standard error to `path` (truncated, like upstream).
#[cfg(unix)]
pub fn start(path: &Path) -> Result<()> {
    use std::{
        io::{Read, Write},
        os::fd::FromRawFd,
    };
    let mut file = std::fs::File::create(path)?;
    let mut fds = [0; 2];
    // SAFETY: plain POSIX calls on descriptors owned by this function.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(error("cannot create log pipe"));
        }
        let saved = libc::dup(2);
        if saved < 0 || libc::dup2(fds[1], 2) < 0 {
            return Err(error("cannot redirect standard error"));
        }
        libc::close(fds[1]);
        let mut reader = std::fs::File::from_raw_fd(fds[0]);
        let mut terminal = std::fs::File::from_raw_fd(libc::dup(saved));
        let thread = std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let _ = terminal.write_all(&buffer[..n]);
                let _ = file.write_all(&buffer[..n]);
            }
            let _ = file.flush();
        });
        *TEE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Tee { saved, thread });
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn start(_: &Path) -> Result<()> {
    Err(error("--write-log is only supported on Unix-like systems"))
}

/// Restore standard error and wait until everything is written. Call before
/// exiting the process.
pub fn finish() {
    let Some(tee) = TEE.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    #[cfg(unix)]
    // SAFETY: restores the descriptor saved by `start`, closing the pipe's
    // write end so the copying thread sees end-of-file.
    unsafe {
        libc::dup2(tee.saved, 2);
        libc::close(tee.saved);
    }
    let _ = tee.thread.join();
}
