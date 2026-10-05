//! A Rust framework for ESP-IDF NimBLE with no public BLE API or runtime
//! behavior yet. Private tooling validates consumer build context and generates
//! bindings from the selected ESP-IDF configuration; generated declarations
//! and C shims remain private. A reusable CMake module builds an application's
//! Rust static library with this crate for ESP32-C3/S3 inside `idf.py`; real
//! target builds check Rust layouts against the consumer's GCC. Azure compiles
//! and links generic fixtures but does not run them. See the integration,
//! build-context, and binding-generation guides for evidence limits.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
