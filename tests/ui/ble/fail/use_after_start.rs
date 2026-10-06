//! Starting consumes the configuring owner, so it cannot be used afterwards.

use argyle_nimble::gatt::GattServer;
use argyle_nimble::{Access, Ble, Configuring};
use std::time::Duration;

fn start_then_configure(ble: Ble<Configuring>, server: GattServer) {
    let _running = ble.start(server, Access::Open);
    let _ = ble.sync_timeout(Duration::from_secs(1));
}

fn main() {}
