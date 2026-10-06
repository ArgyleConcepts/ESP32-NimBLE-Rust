//! The frozen server's services are not reachable through any public method.

use argyle_nimble::gatt::{GattServer, Service};
use argyle_nimble::Uuid;

fn main() {
    let server = GattServer::new([Service::primary(Uuid::Uuid16(0x181d))]).unwrap();
    let _ = server.services();
}
