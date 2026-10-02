//! Private boundary reserved for backend behavior and ESP-IDF integration.
//!
//! The C shims here expose only a narrow, audited NimBLE surface to private
//! generated bindings. No safe runtime backend or public BLE API exists yet.
//! Generated bindings must come from the consuming application's actual
//! ESP-IDF build configuration and remain private.
