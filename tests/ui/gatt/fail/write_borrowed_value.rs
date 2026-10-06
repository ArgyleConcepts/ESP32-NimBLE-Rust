//! Written values must own their data, so a handler never keeps a borrow of
//! the received native buffer.

use argyle_nimble::gatt::{Characteristic, Writable};
use argyle_nimble::{AttError, Uuid};

struct Name;

impl Characteristic for Name {
    type Value = &'static str;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2a00)
    }
}

impl Writable for Name {
    fn write(&self, _: &'static str) -> Result<(), AttError> {
        Ok(())
    }
}

fn main() {}
