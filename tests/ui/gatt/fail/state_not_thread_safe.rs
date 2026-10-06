//! Handlers run on the BLE host task, so their state must be thread-safe.

use argyle_nimble::gatt::Characteristic;
use argyle_nimble::Uuid;
use std::cell::Cell;
use std::rc::Rc;

struct Counter(Rc<Cell<u32>>);

impl Characteristic for Counter {
    type Value = u32;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff2)
    }
}

fn main() {}
