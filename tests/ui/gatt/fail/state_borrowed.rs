//! Characteristics outlive the code that defines them, so they cannot retain
//! borrowed state.

use argyle_nimble::gatt::Characteristic;
use argyle_nimble::Uuid;
use std::sync::atomic::AtomicU8;

struct Level<'a>(&'a AtomicU8);

impl<'a> Characteristic for Level<'a> {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2a19)
    }
}

fn main() {}
