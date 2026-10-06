//! A readable descriptor value must have a wire encoding.

use argyle_nimble::gatt::{Descriptor, ReadableDescriptor};
use argyle_nimble::{AttError, Uuid};

struct Raw {
    flags: u32,
}

struct Status;

impl Descriptor for Status {
    type Value = Raw;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

impl ReadableDescriptor for Status {
    fn read(&self) -> Result<Raw, AttError> {
        Ok(Raw { flags: 0 })
    }
}

fn main() {}
