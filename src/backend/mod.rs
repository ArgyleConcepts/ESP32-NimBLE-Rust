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
