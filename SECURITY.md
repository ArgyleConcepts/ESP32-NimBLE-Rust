# Security policy

## Reporting a vulnerability

The project's reporting channel is GitHub
[private vulnerability reporting](https://github.com/ArgyleConcepts/ESP32-Nimble-Rust/security/advisories/new).
Do not disclose a vulnerability in a public issue or pull request.

Private reporting is enabled. If the
link is unavailable, contact [David](https://github.com/david-cyman-argyle)
through a private contact option on his profile to arrange a private channel.
Do not send vulnerability details until that channel is established. Enabling
and periodically checking GitHub private reporting is part of repository maintenance.

Include the affected version or commit, target/ESP-IDF configuration when
relevant, a minimal reproduction, impact, and any suggested fix. Remove real
credentials, personal data, and proprietary firmware. Maintainers will coordinate
investigation, remediation, and disclosure privately. Response times depend on
maintainer availability.

## Supported code

There is no published release or implemented BLE API yet. Fixes target current
development code; there is no promised backport or long-term support commitment.
See the [README](README.md) for implementation and verification status. Planned
open-access BLE behavior does not provide pairing, bonding, or encrypted access.
