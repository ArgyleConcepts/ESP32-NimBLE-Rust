//! A readable value must have a wire encoding; Rust layouts are never sent.

use argyle_nimble::gatt::{Characteristic, Readable};
use argyle_nimble::{AttError, Uuid};

#[derive(Clone, Copy)]
struct Raw {
    a: u8,
    b: u32,
}

struct Status;

impl Characteristic for Status {
    type Value = Raw;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff1)
    }
}

impl Readable for Status {
    fn read(&self) -> Result<Raw, AttError> {
        Ok(Raw { a: 1, b: 2 })
    }
}

fn main() {}
