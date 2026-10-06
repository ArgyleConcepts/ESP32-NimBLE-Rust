//! Starting requires an explicit access choice.

use argyle_nimble::gatt::GattServer;
use argyle_nimble::{Ble, Configuring};

fn start(ble: Ble<Configuring>, server: GattServer) {
    let _ = ble.start(server);
}

fn main() {}
