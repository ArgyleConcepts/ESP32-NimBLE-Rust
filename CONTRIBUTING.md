# Contributing

Contributions to code, tests, examples, and documentation are welcome. Read the
[README](README.md) for planned versus available functionality.

## Discuss a change

Search [issues](https://github.com/ArgyleConcepts/ESP32-Nimble-Rust/issues) and
open PRs before starting. For a bug, include a minimal reproduction, expected and
actual behavior, package version or commit, and relevant toolchain/ESP-IDF
versions and configuration. Discuss large features or breaking API changes
first. Questions can be issues; no internal ticket or company account is needed.

Use generic examples. Do not submit credentials, personal data, proprietary
firmware, or private product UUID inventories. Follow [SECURITY.md](SECURITY.md)
for vulnerabilities and the [Code of Conduct](CODE_OF_CONDUCT.md).

## AI-assisted contributions

AI-assisted contributions are welcome. You are responsible for everything you
submit, whether you wrote it yourself or used an AI tool. Read and understand
the entire change, verify generated claims and references, and ensure you have
the right to contribute all included material under the MIT license.

Be prepared to explain the design, what tests demonstrate, and unverified
behavior. Follow the Azure validation process below. You remain responsible for
reviewer questions and fixes; AI assistance does not transfer that responsibility
to the tool or maintainers.

## Branches and implementation

The default contribution branch and PR target is `develop`; `master` is for
maintainer-managed release preparation. Use a fork and a concise branch name
describing the work, without `codex/` or other agent-specific prefixes. Keep PRs
focused and do not push directly to the long-lived branches.

Both branches are protected, including for administrators. The former default
`main` remains read-only as a bootstrap reference; do not target contributions there.

Start with the single library crate. Introduce public APIs with implementation
and tests, without placeholder APIs. Keep unsafe code and generated bindings
inside the private backend boundary; do not add `esp-idf-sys` or commit binding
snapshots. Document API contracts, errors, ownership, and examples in rustdoc
beside the API. Reserve standalone architecture docs for cross-cutting decisions
and link to authoritative policy instead of duplicating it.

## Validation and pull requests

All project builds/tests run through Azure DevOps's self-hosted `macOS` pool.
Do not substitute local project builds/tests or GitHub Actions. Contributors
need no Azure credentials. Direct fork builds must remain disabled: a maintainer
reviews external changes, including executable build inputs, and promotes them
to a trusted repository branch for validation. See the
[maintainer guide](docs/MAINTAINING.md).

The [host pipeline and pinned tools](docs/CI.md) are available. Describe checks
actually performed. Maintainers link the Azure run for the promoted commit and
relay failures to external contributors, who need no Azure access. Azure's C3/S3
jobs build and link generic `idf.py` fixtures; that is compile/link evidence, not
on-device execution. File inspection or an empty test harness is not BLE behavior
or target verification. Each behavior change needs appropriate positive,
negative, and failure-path tests as part of its implementation.

PRs should explain the problem and resulting behavior, link public issues,
identify breaking changes, and include Azure run links when available. Update
docs/examples for API changes. Draft PRs are welcome.

Merging requires an up-to-date branch,
the `Argyle NimBLE` Azure check, resolved review conversations, and
designated code-owner approval, subject to the documented review-only exception.
[CODEOWNERS](https://github.com/ArgyleConcepts/ESP32-Nimble-Rust/blob/HEAD/.github/CODEOWNERS)
assigns all files to `@david-cyman-argyle`.

## License and releases

Contributions use the existing [MIT license](LICENSE). Retain its Argyle Concepts
copyright and required third-party attribution. Publication is a separate
maintainer decision; validation must not publish packages. Publishing credentials
do not belong in source control or PR validation.
