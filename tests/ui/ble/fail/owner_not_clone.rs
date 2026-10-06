//! There is exactly one owner: neither state can be cloned.

use argyle_nimble::{Ble, Configuring, Running};

fn copies(configuring: &Ble<Configuring>, running: &Ble<Running>) {
    let _ = <Ble<Configuring> as Clone>::clone(configuring);
    let _ = <Ble<Running> as Clone>::clone(running);
}

fn main() {}
