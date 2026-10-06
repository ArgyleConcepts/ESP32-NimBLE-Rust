//! Declaring descriptor write access requires a `WritableDescriptor` handler.

use argyle_nimble::gatt::{Descriptor, DescriptorDef};
use argyle_nimble::Uuid;

struct Label;

impl Descriptor for Label {
    type Value = String;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

fn main() {
    let _ = DescriptorDef::new(Label).unwrap().writable();
}
