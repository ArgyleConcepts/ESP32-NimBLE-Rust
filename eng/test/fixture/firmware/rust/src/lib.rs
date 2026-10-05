//! Generic compile/link fixture for argyle-nimble's Cargo/idf.py integration.
//! It has no BLE behavior and is never flashed by validation.

// Link the dependency even though it exposes no public API yet; its private
// bindings, shim archive, and link audit root must reach the firmware link.
extern crate argyle_nimble;

#[cfg(any(feature = "std-audit", feature = "lstat-audit"))]
mod std_audit;

/// Entry point called from the fixture's C `app_main`.
#[no_mangle]
pub extern "C" fn argyle_nimble_link_fixture_entry() -> u32 {
    0
}
