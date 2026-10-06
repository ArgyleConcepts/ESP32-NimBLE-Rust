//! Compile-time contracts of the public API.
//!
//! `pass` fixtures must compile and run; each `fail` fixture must be rejected
//! with the compiler error recorded beside it. Expected errors come from the
//! pinned host toolchain in `rust-toolchain.toml`; regenerate them with
//! `TRYBUILD=overwrite` only after checking that each new message still
//! reports the intended violation. One harness builds every fixture so the
//! cases share trybuild's build directory without contention.

#[test]
fn compile_contracts() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/gatt/pass/*.rs");
    cases.compile_fail("tests/ui/gatt/fail/*.rs");
    cases.pass("tests/ui/ble/pass/*.rs");
    cases.compile_fail("tests/ui/ble/fail/*.rs");
}
