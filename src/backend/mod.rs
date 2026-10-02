//! Private boundary reserved for backend behavior and ESP-IDF integration.
//!
//! The C shims here expose only a narrow, audited NimBLE surface to private
//! generated bindings. No safe runtime backend or public BLE API exists yet.
//! Generated bindings must come from the consuming application's actual
//! ESP-IDF build configuration and remain private.

#[cfg(argyle_nimble_esp)]
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/nimble_bindings.rs"));
}
