//! A notification endpoint cannot be assembled from its parts.

#![allow(unreachable_code)]

use argyle_nimble::gatt::NotifyEndpoint;
use std::marker::PhantomData;

fn main() {
    let _ = NotifyEndpoint::<u8> { id: todo!(), value: PhantomData };
}
