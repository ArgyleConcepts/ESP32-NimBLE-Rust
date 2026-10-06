//! Every supported capability combination compiles when its handlers exist.

use argyle_nimble::codec::{Decode, DecodeError, Encode, EncodeError, ValueReader, ValueWriter};
use argyle_nimble::gatt::{Characteristic, CharacteristicDef, GattServer, Readable, Service, Writable};
use argyle_nimble::{AttError, Uuid};

struct ReadOnly;
impl Characteristic for ReadOnly {
    type Value = u16;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xa001)
    }
}
impl Readable for ReadOnly {
    fn read(&self) -> Result<u16, AttError> {
        Ok(1)
    }
}

struct WriteOnly;
impl Characteristic for WriteOnly {
    type Value = Vec<u8>;
    const MAX_LEN: usize = 20;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xa002)
    }
}
impl Writable for WriteOnly {
    fn write(&self, _: Vec<u8>) -> Result<(), AttError> {
        Ok(())
    }
}

struct ReadWrite;
impl Characteristic for ReadWrite {
    type Value = String;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xa003)
    }
}
impl Readable for ReadWrite {
    fn read(&self) -> Result<String, AttError> {
        Ok(String::new())
    }
}
impl Writable for ReadWrite {
    fn write(&self, _: String) -> Result<(), AttError> {
        Ok(())
    }
}

/// An application codec type used as a characteristic value.
struct Sample {
    centidegrees: i16,
}
impl Encode for Sample {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write(&self.centidegrees)
    }
}
impl Decode<'_> for Sample {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        Ok(Self { centidegrees: reader.read()? })
    }
}

struct NotifyOnly;
impl Characteristic for NotifyOnly {
    type Value = Sample;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xa004)
    }
}

struct Everything;
impl Characteristic for Everything {
    type Value = Sample;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid128(0x0000_a005_0000_1000_8000_0080_5f9b_34fb)
    }
}
impl Readable for Everything {
    fn read(&self) -> Result<Sample, AttError> {
        Ok(Sample { centidegrees: -40 })
    }
}
impl Writable for Everything {
    fn write(&self, sample: Sample) -> Result<(), AttError> {
        let _ = sample.centidegrees;
        Ok(())
    }
}

fn main() -> Result<(), argyle_nimble::Error> {
    let (notify_only, _samples) = CharacteristicDef::new(NotifyOnly).notifiable();
    let (read_notify, _counts) = CharacteristicDef::new(ReadOnly).readable().notifiable();
    let (everything, _everything) = CharacteristicDef::new(Everything)
        .readable()
        .writable()
        .writable_without_response()
        .notifiable();
    let first = Service::primary(Uuid::Uuid16(0x1810))
        .characteristic(CharacteristicDef::new(ReadOnly).readable())
        .characteristic(CharacteristicDef::new(WriteOnly).writable())
        .characteristic(CharacteristicDef::new(WriteOnly).writable_without_response())
        .characteristic(CharacteristicDef::new(ReadWrite).readable().writable());
    let second = Service::primary(Uuid::Uuid16(0x1811))
        .characteristic(notify_only)
        .characteristic(read_notify)
        .characteristic(everything);
    let _server = GattServer::new([first, second])?;
    Ok(())
}
