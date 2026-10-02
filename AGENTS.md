# Contributor agent instructions

Read [CONTRIBUTING.md](CONTRIBUTING.md) and
[docs/MAINTAINING.md](docs/MAINTAINING.md) before changes.

## Git branches

- Never add a `codex/` prefix, or any other agent-specific prefix, to branch
  names. Use a concise branch name describing the work.
- Follow the documented branch setup status and protected-branch PR workflow.

## Implementation and validation

- Keep the crate minimal until a task introduces behavior. Do not invent public
  placeholder APIs or tests that only assert the skeleton exists.
- Keep unsafe/FFI and generated bindings private. Generate bindings from actual
  consumer ESP-IDF configuration; do not add `esp-idf-sys` or binding snapshots.
- Run all project builds/tests through Azure's self-hosted `macOS` pool. Do not
  substitute local builds/tests or GitHub Actions. Report missing Azure evidence
  as pending; metadata/file inspection is not a passing build.
- Include appropriate positive, negative, and failure-path tests with behavior.
  Document API contracts beside code and link to authoritative policy.
- Use generic examples and public references; never copy proprietary firmware,
  private UUID inventories, or credentials into the repository.
- Distinguish planned from implemented behavior. Do not claim compatibility,
  safety guarantees, or hardware verification without evidence.
- Publication requires a separate decision; retain `publish = false` until the
  approved release process changes it.

## Ownership

`@david-cyman-argyle` owns all code and policy files. Follow the contributor and
maintainer guides for reviews, external-contribution promotion, and reporting.
Documentation does not configure remote settings.
