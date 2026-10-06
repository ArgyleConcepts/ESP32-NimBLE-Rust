//! The connection handler runs on the BLE host task, so its state must be
//! thread-safe.

use argyle_nimble::{Ble, Configuring, ConnectionEvent};
use std::cell::Cell;
use std::rc::Rc;

fn configure(ble: Ble<Configuring>) {
    let count = Rc::new(Cell::new(0_u32));
    let _ = ble.connection_handler(move |_: ConnectionEvent| count.set(count.get() + 1));
}

fn main() {}
