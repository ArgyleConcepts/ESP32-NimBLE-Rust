//! A Rust framework for ESP-IDF NimBLE with no public BLE API or runtime
//! behavior yet. Private tooling validates consumer build context and generates
//! bindings from the selected ESP-IDF configuration; generated declarations
//! and C shims remain private. Generic host fixtures do not establish ESP ABI
//! compatibility. Genuine C3/S3 header-generation checks and full firmware
//! compile/link integration remain pending. See the build-context and
//! binding-generation contracts for the current evidence and limitations.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
