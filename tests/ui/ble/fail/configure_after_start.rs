//! Configuration is only available before the host starts.

use argyle_nimble::{Ble, Running};
use std::time::Duration;

fn reconfigure(running: Ble<Running>) {
    let _ = running.sync_timeout(Duration::from_secs(1));
}

fn main() {}
