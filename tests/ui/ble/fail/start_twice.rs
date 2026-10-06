//! A running owner cannot be started again.

use argyle_nimble::gatt::GattServer;
use argyle_nimble::{Access, Ble, Running};

fn restart(running: Ble<Running>, server: GattServer) {
    let _ = running.start(server, Access::Open);
}

fn main() {}
