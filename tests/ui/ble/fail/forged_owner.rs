//! An owner can only come from `Ble::take`.

use argyle_nimble::{Ble, Running};

fn main() {
    let _: Ble<Running> = Ble { state: todo!() };
}
