//! Reference std facilities whose ESP-IDF support has needed C compatibility
//! shims, so the link shows which symbols ESP-IDF 6.1 Newlib provides itself.
//! Thread spawn/join and TLS destructors cover the historical `atexit` shim.

use std::cell::Cell;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        COUNT.with(|count| count.set(count.get() + 1));
    }
}

thread_local! {
    static GUARD: Guard = const { Guard };
    static COUNT: Cell<u32> = const { Cell::new(0) };
}

/// Exercise threads, TLS destructors, time, mutexes, and `stat` metadata.
#[no_mangle]
pub extern "C" fn argyle_nimble_link_fixture_std_audit() -> u32 {
    let joined = std::thread::spawn(|| GUARD.with(|_| 1u32))
        .join()
        .unwrap_or(0);
    let total = Mutex::new(joined);
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as u32);
    let started = Instant::now();
    let metadata = std::fs::metadata("/argyle-nimble-missing").is_ok() as u32;
    let elapsed = started.elapsed().as_micros() as u32;
    let value = *total.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    value
        .wrapping_add(seconds)
        .wrapping_add(metadata)
        .wrapping_add(elapsed)
}

/// Rust std implements `symlink_metadata` with POSIX `lstat`, which ESP-IDF 6.1
/// Newlib does not provide. Linking this requires an application-owned shim.
#[cfg(feature = "lstat-audit")]
#[no_mangle]
pub extern "C" fn argyle_nimble_link_fixture_lstat_audit() -> u32 {
    std::fs::symlink_metadata("/argyle-nimble-missing").is_ok() as u32
}
