//! Compile-time contracts of the GATT authoring API.
//!
//! `pass` fixtures must compile; each `fail` fixture must be rejected with the
//! compiler error recorded beside it. Expected errors come from the pinned
//! host toolchain in `rust-toolchain.toml`; regenerate them with
//! `TRYBUILD=overwrite` only after checking that each new message still
//! reports the intended violation.

#[test]
fn gatt_authoring_contracts() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/gatt/pass/*.rs");
    cases.compile_fail("tests/ui/gatt/fail/*.rs");
}
