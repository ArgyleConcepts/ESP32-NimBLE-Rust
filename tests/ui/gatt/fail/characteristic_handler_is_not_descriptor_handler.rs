//! A characteristic read handler is not a descriptor read handler, so a type
//! with both roles cannot route descriptor reads to its characteristic logic.

use argyle_nimble::gatt::{Characteristic, Descriptor, DescriptorDef, Readable};
use argyle_nimble::{AttError, Uuid};

struct Both;

impl Characteristic for Both {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff0)
    }
}

impl Readable for Both {
    fn read(&self) -> Result<u8, AttError> {
        Ok(0)
    }
}

impl Descriptor for Both {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff1)
    }
}

fn main() {
    let _ = DescriptorDef::new(Both).unwrap().readable();
}
