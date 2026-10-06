//! A write handler cannot borrow the request's bytes: NimBLE frees the request
//! buffer after the callback, so written values must be owned.

use argyle_nimble::gatt::{Characteristic, Writable};
use argyle_nimble::{AttError, Uuid};

struct Raw;

impl Characteristic for Raw {
    type Value = &'static [u8];
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff4)
    }
}

impl Writable for Raw {
    fn write(&self, _: &'static [u8]) -> Result<(), AttError> {
        Ok(())
    }
}

fn main() {}
