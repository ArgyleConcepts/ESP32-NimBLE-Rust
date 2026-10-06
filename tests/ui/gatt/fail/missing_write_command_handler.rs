//! Declaring write commands requires a `Writable` handler.

use argyle_nimble::gatt::{Characteristic, CharacteristicDef};
use argyle_nimble::Uuid;

struct Level;

impl Characteristic for Level {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2a19)
    }
}

fn main() {
    let _ = CharacteristicDef::new(Level).writable_without_response();
}
