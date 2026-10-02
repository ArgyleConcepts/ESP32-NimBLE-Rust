//! A Rust framework for ESP-IDF NimBLE, currently at the crate skeleton stage.
//!
//! There are no public BLE APIs yet. This library has no dependencies and does
//! not require ESP-IDF or generated bindings for host compilation. Host and
//! target build verification will run through the project's Azure pipeline.
//!
//! Planned APIs describe services, characteristics, and descriptors using
//! application-owned structs and traits registered with one BLE controller.
//! ESP-IDF integration and generated bindings will remain private.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
