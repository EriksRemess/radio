//! Minimal Linux signal handling without external crates.
//! The handler only sets an atomic flag; cleanup happens on the main thread.

#[cfg(not(target_os = "linux"))]
compile_error!("linein-radio currently supports Linux (ALSA) only");

use std::{
    io,
    os::raw::c_int,
    sync::atomic::{AtomicBool, Ordering},
};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: c_int) {
    STOP.store(true, Ordering::Relaxed);
}

unsafe extern "C" {
    // Linux represents SIG_DFL, SIG_IGN and SIG_ERR as sentinel handler values.
    fn signal(number: c_int, handler: usize) -> usize;
}

pub fn install() -> io::Result<()> {
    for number in [2, 15] {
        // SIGINT, SIGTERM on Linux.
        // SAFETY: stop has the C signal-handler ABI, lives for the entire process,
        // and performs only a lock-free atomic store. These are Linux signal numbers.
        if unsafe { signal(number, stop as *const () as usize) } == usize::MAX {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}
