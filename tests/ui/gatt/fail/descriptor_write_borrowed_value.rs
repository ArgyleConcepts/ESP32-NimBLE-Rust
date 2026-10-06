//! Written descriptor values must own their data.

use argyle_nimble::gatt::{Descriptor, WritableDescriptor};
use argyle_nimble::{AttError, Uuid};

struct Label;

impl Descriptor for Label {
    type Value = &'static str;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

impl WritableDescriptor for Label {
    fn write(&self, _: &'static str) -> Result<(), AttError> {
        Ok(())
    }
}

fn main() {}
