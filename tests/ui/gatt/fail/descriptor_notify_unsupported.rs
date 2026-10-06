//! Descriptors have no notify access; subscriptions belong to the
//! stack-managed CCCD of a notify-capable characteristic.

use argyle_nimble::gatt::{Descriptor, DescriptorDef};
use argyle_nimble::Uuid;

struct Label;

impl Descriptor for Label {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

fn main() {
    let _ = DescriptorDef::new(Label).unwrap().notifiable();
}
