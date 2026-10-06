//! A codec cannot keep the bytes it decodes from beyond the decode call:
//! they may be a request buffer that is released afterwards.

use argyle_nimble::codec::{Decode, DecodeError, ValueReader};
use std::sync::Mutex;

static RETAINED: Mutex<Vec<&'static [u8]>> = Mutex::new(Vec::new());

struct Payload;

impl<'a> Decode<'a> for Payload {
    fn decode(reader: &mut ValueReader<'a>) -> Result<Self, DecodeError> {
        RETAINED.lock().unwrap().push(reader.read_remaining());
        Ok(Self)
    }
}

fn main() {}
