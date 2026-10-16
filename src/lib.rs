//! Shared Unix readiness polling for the server and terminal client.

#[cfg(not(unix))]
compile_error!("smallchat-rs currently supports Unix platforms (macOS and Linux)");

use std::io;

/// Wait for socket or terminal readiness. An interrupted wait is harmless:
/// callers simply start their next event-loop iteration.
pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<()> {
    // SAFETY: the slice owns fds.len() valid pollfd entries for the whole call.
    let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}
