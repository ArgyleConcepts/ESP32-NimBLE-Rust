//! State may be sent to the host task but is also used concurrently, so it
//! must be `Sync` as well as `Send`.

use argyle_nimble::gatt::Characteristic;
use argyle_nimble::Uuid;
use std::cell::Cell;

struct Counter(Cell<u32>);

impl Characteristic for Counter {
    type Value = u32;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0xfff3)
    }
}

fn main() {}
