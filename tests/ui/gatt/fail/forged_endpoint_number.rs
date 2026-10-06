//! A notification endpoint cannot be converted from a number or handle.

use argyle_nimble::gatt::NotifyEndpoint;

fn main() {
    let _: NotifyEndpoint<u8> = NotifyEndpoint::from(42_u16);
}
