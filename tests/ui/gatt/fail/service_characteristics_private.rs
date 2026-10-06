//! A service's registered characteristics are not reachable publicly.

use argyle_nimble::gatt::Service;
use argyle_nimble::Uuid;

fn main() {
    let service = Service::primary(Uuid::Uuid16(0x1801));
    let _ = service.characteristics();
}
