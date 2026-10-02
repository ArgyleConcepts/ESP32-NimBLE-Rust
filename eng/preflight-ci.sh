#!/usr/bin/env bash
# Reject inherited build configuration without printing its contents or values.
set -euo pipefail

while IFS= read -r variable; do
  case "$variable" in
    RUSTUP_HOME|CARGO_HOME|CARGO_TARGET_DIR|CARGO_TERM_COLOR|RUSTDOCFLAGS|\
    UV_CACHE_DIR|UV_PYTHON_INSTALL_DIR|UV_PYTHON_BIN_DIR|UV_NO_CONFIG) ;;
    RUSTFLAGS|CARGO_ENCODED_RUSTFLAGS|CARGO_ENCODED_RUSTDOCFLAGS|\
    RUSTC|RUSTDOC|RUSTC_*|RUSTUP_*|CARGO_*|UV_*)
      printf 'Refusing inherited tool configuration: %s\n' "$variable" >&2
      exit 1
      ;;
  esac
done < <(compgen -e)

ancestor=$(dirname "$(pwd -P)")
while :; do
  for name in config config.toml; do
    candidate="$ancestor/.cargo/$name"
    if [[ -e "$candidate" || -L "$candidate" ]]; then
      printf 'Refusing inherited Cargo configuration: %s\n' "$candidate" >&2
      exit 1
    fi
  done
  [[ "$ancestor" == / ]] && break
  ancestor=$(dirname "$ancestor")
done
printf 'No inherited tool overrides or ancestor Cargo configuration\n'
