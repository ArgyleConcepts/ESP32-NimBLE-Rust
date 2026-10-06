//! A Rust framework for ESP-IDF NimBLE.
//!
//! The public API so far covers the values that cross the BLE boundary:
//!
//! - [`Uuid`]: 16-, 32-, and 128-bit Bluetooth UUIDs with checked parsing and
//!   explicit canonical and wire byte orders.
//! - [`codec`]: explicit wire encodings for attribute values and the
//!   [`Encode`](codec::Encode)/[`Decode`](codec::Decode) contract for
//!   application codecs, over Rust-owned bytes only.
//! - [`AttError`] for ATT results returned to a client, and [`Error`] for
//!   framework failures; the two are deliberately separate.
//! - [`gatt`]: declarative services, characteristics, and custom descriptors
//!   whose capabilities are checked at compile time, frozen into a
//!   [`gatt::GattServer`] definition.
//!
//! There is no BLE controller, NimBLE registration, or runtime behavior yet. Private
//! tooling validates consumer build context and generates bindings from the
//! selected ESP-IDF configuration; generated declarations and C shims remain
//! private. A reusable CMake module builds an application's Rust static
//! library with this crate for ESP32-C3/S3 inside `idf.py`; real target builds
//! check Rust layouts against the consumer's GCC. Azure compiles and links
//! generic fixtures but does not run them. See the integration, build-context,
//! and binding-generation guides for evidence limits.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
pub mod codec;
mod error;
pub mod gatt;
mod uuid;

pub use error::{AttError, BackendError, Error, ErrorKind, InvalidAttErrorCode};
pub use uuid::{Uuid, UuidBytes, UuidError, BLUETOOTH_BASE_UUID};
