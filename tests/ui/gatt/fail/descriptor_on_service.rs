//! A descriptor attaches to a characteristic, never directly to a service.

use argyle_nimble::gatt::{Descriptor, DescriptorDef, ReadableDescriptor, Service};
use argyle_nimble::{AttError, Uuid};

struct Label;

impl Descriptor for Label {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

impl ReadableDescriptor for Label {
    fn read(&self) -> Result<u8, AttError> {
        Ok(0)
    }
}

fn main() {
    let descriptor = DescriptorDef::new(Label).unwrap().readable();
    let _ = Service::primary(Uuid::Uuid16(0x180f)).characteristic(descriptor);
}
