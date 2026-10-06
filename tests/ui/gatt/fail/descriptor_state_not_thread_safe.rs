//! Descriptor handlers run on the BLE host task, so their state must be
//! thread-safe.

use argyle_nimble::gatt::Descriptor;
use argyle_nimble::Uuid;
use std::cell::Cell;
use std::rc::Rc;

struct Counter(Rc<Cell<u8>>);

impl Descriptor for Counter {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2901)
    }
}

fn main() {}
