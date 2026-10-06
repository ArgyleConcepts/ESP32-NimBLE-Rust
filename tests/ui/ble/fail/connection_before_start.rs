//! There is no connection, and no advertising to start, before the host runs.

use argyle_nimble::{Ble, Configuring};

fn query(ble: Ble<Configuring>) {
    let _ = ble.connection();
    let _ = ble.start_advertising();
}

fn main() {}
