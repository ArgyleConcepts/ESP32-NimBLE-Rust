//! Readable, writable, and read/write descriptors attach to characteristics,
//! including descriptors whose UUIDs are only known at runtime.

use argyle_nimble::gatt::{
    Characteristic, CharacteristicDef, Descriptor, DescriptorDef, GattServer, Readable,
    ReadableDescriptor, Service, WritableDescriptor,
};
use argyle_nimble::{AttError, Uuid};
use std::sync::{Arc, Mutex};

struct Level;
impl Characteristic for Level {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2a19)
    }
}
impl Readable for Level {
    fn read(&self) -> Result<u8, AttError> {
        Ok(50)
    }
}

/// A read-only text label.
struct Label(&'static str);
impl Descriptor for Label {
    type Value = String;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}
impl ReadableDescriptor for Label {
    fn read(&self) -> Result<String, AttError> {
        Ok(self.0.to_owned())
    }
}

/// A write-only trigger.
struct Trigger;
impl Descriptor for Trigger {
    type Value = bool;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid128(0x5c3a_9e21_8b4f_4d6a_a1e7_3f0c_2b9d_7e45)
    }
}
impl WritableDescriptor for Trigger {
    fn write(&self, _: bool) -> Result<(), AttError> {
        Ok(())
    }
}

/// A read/write setting whose UUID comes from configuration.
struct Setting {
    uuid: Uuid,
    value: Arc<Mutex<u16>>,
}
impl Descriptor for Setting {
    type Value = u16;
    const MAX_LEN: usize = 2;
    fn uuid(&self) -> Uuid {
        self.uuid
    }
}
impl ReadableDescriptor for Setting {
    fn read(&self) -> Result<u16, AttError> {
        Ok(*self.value.lock().map_err(|_| AttError::UNLIKELY)?)
    }
}
impl WritableDescriptor for Setting {
    fn write(&self, value: u16) -> Result<(), AttError> {
        *self.value.lock().map_err(|_| AttError::UNLIKELY)? = value;
        Ok(())
    }
}

fn main() -> Result<(), argyle_nimble::Error> {
    let configured: Uuid = "d4e8f1a2-6b3c-4f9e-8a7d-1c2b3e4f5a6b".parse().expect("valid UUID");
    let setting = Setting {
        uuid: configured,
        value: Arc::new(Mutex::new(0)),
    };
    let (level, _updates) = CharacteristicDef::new(Level)
        .readable()
        .descriptor(DescriptorDef::new(Label("battery"))?.readable())
        .descriptor(DescriptorDef::new(Trigger)?.writable())
        .descriptor(DescriptorDef::new(setting)?.readable().writable())
        .notifiable();
    let _server = GattServer::new([Service::primary(Uuid::Uuid16(0x180f)).characteristic(level)])?;
    Ok(())
}
