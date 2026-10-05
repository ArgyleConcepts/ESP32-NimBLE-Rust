#!/usr/bin/env bash
# Install the pinned Espressif Rust toolchain into this Azure job's RUSTUP_HOME.
# The archives and digests come from eng/rust-target-toolchain.lock.json; any
# mismatch fails closed and no other toolchain is substituted.
set -euo pipefail

if [[ "${TF_BUILD:-}" != "True" && "${TF_BUILD:-}" != "true" ]]; then
  printf 'Target Rust toolchain setup is restricted to Azure Pipelines\n' >&2
  exit 1
fi
[[ "$(uname -s)" == Darwin ]] || { printf 'A macOS agent is required\n' >&2; exit 1; }
: "${NIMBLE_TOOLS_DIRECTORY:?Azure must provide NIMBLE_TOOLS_DIRECTORY}"
: "${RUSTUP_HOME:?Azure must provide RUSTUP_HOME}" "${CARGO_HOME:?Azure must provide CARGO_HOME}"
python_bin=$(command -v "${NIMBLE_PYTHON:-python3}") || {
  printf 'Python 3 is required to read the toolchain lock\n' >&2
  exit 1
}
export PATH="$CARGO_HOME/bin:$PATH"

case "$(uname -m)" in
  arm64) host=aarch64-apple-darwin ;;
  x86_64) host=x86_64-apple-darwin ;;
  *) printf 'Unsupported macOS architecture\n' >&2; exit 1 ;;
esac

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
lock="$script_dir/rust-target-toolchain.lock.json"
lock_value() {
  "$python_bin" -c 'import json,sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[key]
print(value)' "$lock" "$@"
}
name=$(lock_value toolchain name)
version=$(lock_value toolchain version)
base=$(lock_value toolchain download_base)
archive=$(lock_value toolchain archives "$host" file)
archive_digest=$(lock_value toolchain archives "$host" sha256)
source_archive=$(lock_value toolchain rust_src file)
source_digest=$(lock_value toolchain rust_src sha256)
[[ "$name" =~ ^[a-z0-9-]+$ ]] || { printf 'Invalid toolchain name in lock\n' >&2; exit 1; }

work="$NIMBLE_TOOLS_DIRECTORY/rust-target"
destination="$work/toolchain"
if [[ -e "$work" ]]; then
  printf 'Refusing to reuse a target Rust toolchain directory: %s\n' "$work" >&2
  exit 1
fi
mkdir -p "$work/download" "$work/extract" "$destination"

for pair in "$archive:$archive_digest" "$source_archive:$source_digest"; do
  file=${pair%%:*}
  digest=${pair#*:}
  curl --fail --location --silent --show-error --retry 3 "$base/$file" --output "$work/download/$file"
  printf '%s  %s\n' "$digest" "$work/download/$file" | shasum -a 256 --check
  tar -xJf "$work/download/$file" -C "$work/extract"
done

installers=()
while IFS= read -r installer; do
  installers+=("$installer")
done < <(find "$work/extract" -mindepth 2 -maxdepth 2 -name install.sh -type f | sort)
[[ ${#installers[@]} -eq 2 ]] || {
  printf 'Expected the compiler and rust-src installers in the pinned archives\n' >&2
  exit 1
}
for installer in "${installers[@]}"; do
  bash "$installer" --destdir="$destination" --prefix='' \
    --without=rust-docs-json-preview,rust-docs --disable-ldconfig
done

rustup toolchain link "$name" "$destination"
rustc_version=$(rustc "+$name" -vV)
printf '%s\n' "$rustc_version"
[[ "$rustc_version" == *"($version)"* ]] || {
  printf 'Installed target Rust toolchain does not report release %s\n' "$version" >&2
  exit 1
}
[[ "$rustc_version" == *"host: $host"* ]] || {
  printf 'Installed target Rust toolchain has an unexpected host\n' >&2
  exit 1
}
cargo "+$name" --version --verbose
targets=$(rustc "+$name" --print target-list)
for target in riscv32imc-esp-espidf xtensa-esp32s3-espidf; do
  grep -qx "$target" <<< "$targets" || {
    printf 'Target Rust toolchain does not support %s\n' "$target" >&2
    exit 1
  }
done
sysroot=$(rustc "+$name" --print sysroot)
[[ -f "$sysroot/lib/rustlib/src/rust/library/Cargo.lock" ]] || {
  printf 'Target Rust toolchain is missing rust-src for -Zbuild-std\n' >&2
  exit 1
}
printf 'Linked Espressif Rust %s as rustup toolchain %s\n' "$version" "$name"
