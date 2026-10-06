//! Definitions move into the service and the service into the server, so the
//! structure cannot change after the server is built.

use argyle_nimble::gatt::{Characteristic, CharacteristicDef, GattServer, Readable, Service};
use argyle_nimble::{AttError, Uuid};

struct Level;

impl Characteristic for Level {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2a19)
    }
}

impl Readable for Level {
    fn read(&self) -> Result<u8, AttError> {
        Ok(0)
    }
}

fn main() {
    let service = Service::primary(Uuid::Uuid16(0x180f))
        .characteristic(CharacteristicDef::new(Level).readable());
    let _server = GattServer::new([service]).unwrap();
    let _ = service.characteristic(CharacteristicDef::new(Level).readable());
}
