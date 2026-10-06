//! Private boundary between framework logic and ESP-IDF NimBLE.
//!
//! - [`native`]: the crate-private [`native::Backend`] trait over the bound
//!   native operations, with typed errors and events.
//! - [`dispatch`]: delivery of native callbacks to the framework, including
//!   quiescent detach for shutdown.
//! - [`gap`]: translation of the C shim's GAP event view into owned events.
//! - [`mbuf`]: owned native buffers that are freed or transferred exactly once.
//! - `esp` (ESP builds only): the real backend, the only caller of the
//!   generated bindings.
//! - `fake` (`cfg(test)` only): a deterministic host backend for tests.
//! - `unavailable` (non-ESP builds only): an uninhabited backend, so builds
//!   without NimBLE cannot construct a running host.
//!
//! The public [`Ble`](crate::Ble) owner drives the host lifecycle through
//! this boundary; GATT registration and the connection runtime build on it in
//! later work. Generated bindings come from the consuming application's
//! actual ESP-IDF build configuration and remain private.

// Consumed by the controller and GATT runtime in later tickets; host tests
// exercise these modules today.
#![cfg_attr(not(test), allow(dead_code))]

pub(crate) mod dispatch;
pub(crate) mod gap;
pub(crate) mod mbuf;
pub(crate) mod native;

#[cfg(argyle_nimble_esp)]
pub(crate) mod esp;
#[cfg(argyle_nimble_esp)]
mod esp_gatt;

#[cfg(test)]
pub(crate) mod fake;

#[cfg(not(argyle_nimble_esp))]
pub(crate) mod unavailable;

#[cfg(argyle_nimble_esp)]
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/nimble_bindings.rs"));

    // Real ESP target builds only: rustc checks the generated records and C
    // scalar types against sizes, alignments, and offsets reported by the
    // consumer's selected GCC. A mismatch fails compilation.
    #[cfg(argyle_nimble_target_abi)]
    include!(concat!(env!("OUT_DIR"), "/nimble_layout.rs"));

    // Validation fixtures only: an uncalled linker root that references every
    // bound C function so a retained firmware link must resolve each symbol.
    #[cfg(argyle_nimble_link_audit)]
    include!(concat!(env!("OUT_DIR"), "/nimble_link_audit.rs"));
}
