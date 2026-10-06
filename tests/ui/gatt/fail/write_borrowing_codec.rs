//! An application codec that borrows from the received bytes cannot be a
//! written value: NimBLE frees the request after the callback, so a chunk
//! or frame type must own its payload.

use argyle_nimble::codec::{Decode, DecodeError, ValueReader};
use argyle_nimble::gatt::{Characteristic, Writable};
use argyle_nimble::{AttError, Uuid};

struct Frame<'a> {
    offset: u32,
    payload: &'a [u8],
}

impl<'a> Decode<'a> for Frame<'a> {
    fn decode(reader: &mut ValueReader<'a>) -> Result<Self, DecodeError> {
        Ok(Self {
            offset: reader.read()?,
            payload: reader.read_remaining(),
        })
    }
}

struct Upload;

impl Characteristic for Upload {
    type Value = Frame<'static>;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff8)
    }
}

impl Writable for Upload {
    fn write(&self, frame: Frame<'static>) -> Result<(), AttError> {
        let _ = (frame.offset, frame.payload);
        Ok(())
    }
}

fn main() {}
