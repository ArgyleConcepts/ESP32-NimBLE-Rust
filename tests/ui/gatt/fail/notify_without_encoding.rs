//! Notifications require a value type with a wire encoding.

use argyle_nimble::gatt::{Characteristic, CharacteristicDef};
use argyle_nimble::Uuid;

struct Uptime;

impl Characteristic for Uptime {
    type Value = std::time::Duration;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff0)
    }
}

fn main() {
    let _ = CharacteristicDef::new(Uptime).notifiable();
}
