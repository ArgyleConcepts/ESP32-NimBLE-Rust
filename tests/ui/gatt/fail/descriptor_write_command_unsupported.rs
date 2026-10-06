//! Descriptors have no separate write-without-response access.

use argyle_nimble::gatt::{Descriptor, DescriptorDef, WritableDescriptor};
use argyle_nimble::{AttError, Uuid};

struct Label;

impl Descriptor for Label {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

impl WritableDescriptor for Label {
    fn write(&self, _: u8) -> Result<(), AttError> {
        Ok(())
    }
}

fn main() {
    let _ = DescriptorDef::new(Label).unwrap().writable_without_response();
}
