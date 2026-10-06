//! Written descriptor bytes must be owned, as written characteristic bytes
//! are.

use argyle_nimble::gatt::{Descriptor, WritableDescriptor};
use argyle_nimble::{AttError, Uuid};

struct Raw;

impl Descriptor for Raw {
    type Value = &'static [u8];
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff9)
    }
}

impl WritableDescriptor for Raw {
    fn write(&self, _: &'static [u8]) -> Result<(), AttError> {
        Ok(())
    }
}

fn main() {}
