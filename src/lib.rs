//! A Rust framework for ESP-IDF NimBLE.
//!
//! The public API so far covers the values that cross the BLE boundary, the
//! GATT server and its request handling, and the host that serves it:
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
//!   [`gatt::GattServer`] definition. Handlers serve fixed and
//!   variable-length values within each attribute's `MAX_LEN`, and the module
//!   documents NimBLE's long-value behavior and a generic, application-owned
//!   chunked-transfer pattern.
//! - [`Ble`]: the exclusive owner of the process's BLE host, whose type
//!   tracks configuring and running states, with staged startup, structured
//!   [`StartError`]s, and cleanup that never leaves native code pointing at
//!   freed storage. Starting it registers the GATT server with NimBLE.
//! - [`Advertising`]: checked legacy advertising and scan-response payloads
//!   and the GAP device name, advertised once the host is ready and
//!   restarted by itself unless configured otherwise.
//! - Single-client connection state: a [`ConnectionId`] that a reused native
//!   handle can never revive, the ATT MTU, and per-endpoint subscriptions,
//!   observed through [`Ble::connection`] snapshots and
//!   [`ConnectionEvent`]s delivered to a [`ConnectionHandler`].
//!
//! Notification sending is not implemented yet. Private
//! tooling validates consumer build context and generates bindings from the
//! selected ESP-IDF configuration; generated declarations and C shims remain
//! private. A reusable CMake module builds an application's Rust static
//! library with this crate for ESP32-C3/S3 inside `idf.py`; real target builds
//! check Rust layouts against the consumer's GCC. Azure compiles and links
//! generic fixtures but does not run them, and nothing has been run on
//! hardware. See the integration, build-context, and binding-generation
//! guides for evidence limits.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
pub mod ble;
pub mod codec;
mod error;
pub mod gatt;
mod uuid;

pub use ble::{
    Access, Advertising, AdvertisingBuilder, AdvertisingError, Ble, Cleanup, Configuring,
    ConnectionEvent, ConnectionHandler, ConnectionId, ConnectionInfo, DisconnectReason, LocalName,
    Running, StartError, StartStage,
};
pub use error::{AttError, BackendError, Error, ErrorKind, InvalidAttErrorCode};
pub use uuid::{Uuid, UuidBytes, UuidError, BLUETOOTH_BASE_UUID};
