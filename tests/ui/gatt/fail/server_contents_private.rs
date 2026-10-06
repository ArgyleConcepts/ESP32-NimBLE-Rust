//! The frozen server exposes neither its services nor their native layout.

use argyle_nimble::gatt::{GattServer, Service};
use argyle_nimble::Uuid;

fn main() {
    let mut server = GattServer::new([Service::primary(Uuid::Uuid16(0x1801))]).unwrap();
    let _ = &mut server.services;
}
