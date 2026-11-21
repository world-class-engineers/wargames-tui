//! Simple SIGINT/SIGTERM flag used by the event loop to restore the terminal.
//!
//! Signal handlers must not touch the terminal or allocate; they only flip an
//! atomic flag that the main loop checks on every iteration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

static QUIT_REQUESTED: LazyLock<Arc<AtomicBool>> =
    LazyLock::new(|| Arc::new(AtomicBool::new(false)));

fn flag() -> Arc<AtomicBool> {
    Arc::clone(&QUIT_REQUESTED)
}

/// Register handlers for SIGINT and SIGTERM. Failure is non-fatal: the app
/// still exits normally on Ctrl+Q, and a second signal still terminates it.
pub fn install() -> Result<(), std::io::Error> {
    signal_hook::flag::register(signal_hook::consts::SIGINT, flag())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, flag())?;
    Ok(())
}

pub fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_flag_round_trip() {
        flag().store(false, Ordering::Relaxed);
        assert!(!quit_requested());
        flag().store(true, Ordering::Relaxed);
        assert!(quit_requested());
        flag().store(false, Ordering::Relaxed);
        assert!(!quit_requested());
    }
}
