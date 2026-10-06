//! Descriptors outlive the code that defines them, so they cannot retain
//! borrowed state.

use argyle_nimble::gatt::Descriptor;
use argyle_nimble::Uuid;
use std::sync::atomic::AtomicU8;

struct Setting<'a>(&'a AtomicU8);

impl<'a> Descriptor for Setting<'a> {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

fn main() {}
