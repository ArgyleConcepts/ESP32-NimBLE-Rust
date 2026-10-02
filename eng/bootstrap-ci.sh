#!/usr/bin/env bash
# Run by Azure only. Tools/caches never use the agent account's shared directories.
set -euo pipefail

if [[ "${TF_BUILD:-}" != "True" && "${TF_BUILD:-}" != "true" ]]; then
  printf 'Project tooling and validation must run through Azure Pipelines\n' >&2
  exit 1
fi
[[ "$(uname -s)" == Darwin ]] || { printf 'A macOS agent is required\n' >&2; exit 1; }
: "${NIMBLE_TOOLS_DIRECTORY:?Azure must supply the tools directory}"
: "${RUSTUP_HOME:?}" "${CARGO_HOME:?}" "${UV_PYTHON_INSTALL_DIR:?}"
mkdir -p "$NIMBLE_TOOLS_DIRECTORY" "$CARGO_HOME/bin"

case "$(uname -m)" in
  arm64)
    host=aarch64-apple-darwin
    uv_digest=3f61099e261e449527141dbf125629fab33ad696468c8c90cebbac40185a306c
    ;;
  x86_64)
    host=x86_64-apple-darwin
    uv_digest=76638fdcfa91357858771551a1c88de1f7c3b270b33ab1866f8a0618d9e442d8
    ;;
  *) printf 'Unsupported macOS architecture\n' >&2; exit 1 ;;
esac

uv_archive="$NIMBLE_TOOLS_DIRECTORY/uv.tar.gz"
curl --fail --location --silent --show-error --retry 3 \
  "https://github.com/astral-sh/uv/releases/download/0.8.22/uv-$host.tar.gz" \
  --output "$uv_archive"
printf '%s  %s\n' "$uv_digest" "$uv_archive" | shasum -a 256 --check
tar -xzf "$uv_archive" -C "$NIMBLE_TOOLS_DIRECTORY"
uv="$NIMBLE_TOOLS_DIRECTORY/uv-$host/uv"
case "$("$uv" --version)" in
  'uv 0.8.22'|'uv 0.8.22 ('*) ;;
  *) printf 'Unexpected uv version\n' >&2; exit 1 ;;
esac
"$uv" python install 3.13.7
python_path=$("$uv" python find --managed-python 3.13.7)
[[ "$("$python_path" --version)" == 'Python 3.13.7' ]]
printf '##vso[task.setvariable variable=nimblePython;isReadOnly=true]%s\n' "$python_path"

rustup_init="$NIMBLE_TOOLS_DIRECTORY/rustup-init"
rustup_url="https://static.rust-lang.org/rustup/archive/1.28.2/$host/rustup-init"
curl --fail --location --silent --show-error --retry 3 "$rustup_url" --output "$rustup_init"
curl --fail --location --silent --show-error --retry 3 "$rustup_url.sha256" \
  --output "$NIMBLE_TOOLS_DIRECTORY/rustup-init.sha256"
read -r rustup_digest _ < "$NIMBLE_TOOLS_DIRECTORY/rustup-init.sha256"
printf '%s  %s\n' "$rustup_digest" "$rustup_init" | shasum -a 256 --check
chmod +x "$rustup_init"
"$rustup_init" -y --no-modify-path --profile minimal --default-toolchain 1.90.0
export PATH="$CARGO_HOME/bin:$PATH"
rustup component add --toolchain 1.90.0 rustfmt clippy
printf '##vso[task.prependpath]%s\n' "$CARGO_HOME/bin"
rustup --version
rustc --version
cargo --version
rustfmt --version
cargo clippy --version
"$uv" --version
"$python_path" --version
