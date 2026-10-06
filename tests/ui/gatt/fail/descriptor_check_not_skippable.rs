//! The reserved-UUID check cannot be bypassed: an unchecked construction
//! result cannot be attached.

use argyle_nimble::gatt::{
    Characteristic, CharacteristicDef, Descriptor, DescriptorDef, Readable, ReadableDescriptor,
};
use argyle_nimble::{AttError, Uuid};

struct Level;

impl Characteristic for Level {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2a19)
    }
}

impl Readable for Level {
    fn read(&self) -> Result<u8, AttError> {
        Ok(0)
    }
}

struct Subscriptions;

impl Descriptor for Subscriptions {
    type Value = u16;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2902)
    }
}

impl ReadableDescriptor for Subscriptions {
    fn read(&self) -> Result<u16, AttError> {
        Ok(0)
    }
}

fn main() {
    let _ = CharacteristicDef::new(Level)
        .readable()
        .descriptor(DescriptorDef::new(Subscriptions));
}
