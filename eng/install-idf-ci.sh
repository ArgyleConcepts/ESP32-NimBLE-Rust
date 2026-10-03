#!/usr/bin/env bash
# Install the pinned ESP-IDF SDK and tools into a BuildID/JobID-specific Azure tree.
set -euo pipefail

if [[ "${TF_BUILD:-}" != "True" && "${TF_BUILD:-}" != "true" ]]; then
  printf 'ESP-IDF setup is restricted to Azure Pipelines\n' >&2
  exit 1
fi
[[ "$(uname -s)" == Darwin ]] || { printf 'A macOS agent is required\n' >&2; exit 1; }

: "${BUILD_BUILDID:?Azure must provide BUILD_BUILDID}"
: "${SYSTEM_JOBID:?Azure must provide SYSTEM_JOBID}"
: "${NIMBLE_TOOLS_DIRECTORY:?Azure must provide NIMBLE_TOOLS_DIRECTORY}"
[[ "$BUILD_BUILDID" =~ ^[0-9]+$ ]] || {
  printf 'BUILD_BUILDID must contain only decimal digits\n' >&2
  exit 1
}
[[ "$SYSTEM_JOBID" =~ ^[A-Za-z0-9._-]+$ && "$SYSTEM_JOBID" != . && "$SYSTEM_JOBID" != .. ]] || {
  printf 'SYSTEM_JOBID contains unsupported path characters\n' >&2
  exit 1
}
[[ "$NIMBLE_TOOLS_DIRECTORY" == /* ]] || {
  printf 'NIMBLE_TOOLS_DIRECTORY must be an absolute path\n' >&2
  exit 1
}

for variable in IDF_PATH IDF_TOOLS_PATH IDF_PYTHON_ENV_PATH; do
  if [[ -n "${!variable:-}" ]]; then
    printf 'Refusing inherited ESP-IDF environment: %s\n' "$variable" >&2
    exit 1
  fi
done

case "$(uname -m)" in
  arm64|x86_64) ;;
  *) printf 'Unsupported macOS architecture: %s\n' "$(uname -m)" >&2; exit 1 ;;
esac

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
lock="$script_dir/idf-tools.lock.json"
python_bin=$(command -v "${NIMBLE_PYTHON:-python3}") || {
  printf 'Python 3 is required to install ESP-IDF\n' >&2
  exit 1
}
python_arch=$("$python_bin" -c 'import platform; print(platform.machine())')
[[ "$python_arch" == "$(uname -m)" ]] || {
  printf 'The selected Python architecture does not match the macOS agent\n' >&2
  exit 1
}

job_attempt=${SYSTEM_JOBATTEMPT:-1}
[[ "$job_attempt" =~ ^[0-9]+$ ]] || {
  printf 'SYSTEM_JOBATTEMPT must contain only decimal digits\n' >&2
  exit 1
}
mkdir -p "$NIMBLE_TOOLS_DIRECTORY"
tools_base=$(cd "$NIMBLE_TOOLS_DIRECTORY" && pwd -P)
job_root="$tools_base/idf/$BUILD_BUILDID/$SYSTEM_JOBID/$job_attempt"
if [[ -e "$job_root" ]]; then
  printf 'Refusing to reuse an ESP-IDF job directory: %s\n' "$job_root" >&2
  exit 1
fi
mkdir -p "$job_root"

idf_path="$job_root/esp-idf"
tools_path="$job_root/espressif"
export IDF_PATH="$idf_path"
export IDF_TOOLS_PATH="$tools_path"
export IDF_CCACHE_ENABLE=0
export PIP_CACHE_DIR="$job_root/pip-cache"
export XDG_CACHE_HOME="$job_root/cache"
export PYTHONPYCACHEPREFIX="$job_root/python-cache"
export PYTHONNOUSERSITE=1
export PIP_CONFIG_FILE=/dev/null
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_TERMINAL_PROMPT=0
mkdir -p "$tools_path" "$PIP_CACHE_DIR" "$XDG_CACHE_HOME" "$PYTHONPYCACHEPREFIX"

idf_commit=$("$python_bin" -c 'import json,sys; print(json.load(open(sys.argv[1]))["idf"]["commit"])' "$lock")
idf_repo=$("$python_bin" -c 'import json,sys; print(json.load(open(sys.argv[1]))["idf"]["repository"])' "$lock")
nimble_path=$("$python_bin" -c 'import json,sys; print(json.load(open(sys.argv[1]))["idf"]["nimble_submodule"]["path"])' "$lock")
nimble_commit=$("$python_bin" -c 'import json,sys; print(json.load(open(sys.argv[1]))["idf"]["nimble_submodule"]["commit"])' "$lock")

git init -q "$idf_path"
git -C "$idf_path" remote add origin "$idf_repo"
git -C "$idf_path" fetch --depth=1 origin "$idf_commit"
git -C "$idf_path" checkout --detach FETCH_HEAD
[[ "$(git -C "$idf_path" rev-parse HEAD)" == "$idf_commit" ]] || {
  printf 'ESP-IDF checkout does not match the committed pin\n' >&2
  exit 1
}
git -C "$idf_path" submodule update --init --recursive
nimble_gitlink=$(git -C "$idf_path" ls-tree HEAD -- "$nimble_path" | awk '$1 == "160000" { print $3 }')
nimble_head=$(git -C "$idf_path/$nimble_path" rev-parse HEAD)
[[ "$nimble_gitlink" == "$nimble_commit" && "$nimble_head" == "$nimble_commit" ]] || {
  printf 'NimBLE submodule does not match the committed pin\n' >&2
  exit 1
}

metadata="$idf_path/tools/tools.json"
verified_tools=$("$python_bin" "$script_dir/verify-idf-tools.py" \
  --lock "$lock" --metadata "$metadata" --idf-root "$idf_path" --emit-tools)
targets=$("$python_bin" "$script_dir/verify-idf-tools.py" \
  --lock "$lock" --metadata "$metadata" --idf-root "$idf_path" --emit-targets)
tool_specs=()
while IFS= read -r spec; do
  [[ -n "$spec" ]] && tool_specs+=("$spec")
done <<< "$verified_tools"
[[ ${#tool_specs[@]} -eq 6 ]] || {
  printf 'Expected six pinned ESP-IDF tools\n' >&2
  exit 1
}

idf_tools="$idf_path/tools/idf_tools.py"
"$python_bin" "$idf_tools" --non-interactive install --targets "$targets"
# The required-tool pass also installs target-supported GDB, ULP, OpenOCD, and ROM
# ELF packages from this pinned SDK's tools.json, with its published archive hashes.
# Pin the selected GCC, EspClang/libclang, CMake, and Ninja packages explicitly too.
"$python_bin" "$idf_tools" --non-interactive install --targets "$targets" "${tool_specs[@]}"
# The Python environment uses this SDK revision's core requirements and IDF
# constraints. The SDK does not content-lock every resolved Python wheel.
"$python_bin" "$idf_tools" --non-interactive install-python-env --features core

idf_environment=$("$python_bin" "$idf_tools" --non-interactive export)
eval "$idf_environment"

for spec in "${tool_specs[@]}"; do
  name=${spec%@*}
  version=${spec#*@}
  [[ -d "$tools_path/tools/$name/$version" ]] || {
    printf 'Pinned package was not installed: %s\n' "$spec" >&2
    exit 1
  }
done

clang="$tools_path/tools/esp-clang/esp-21.1.3_20260408/esp-clang/bin/clang"
clang_libs_info="$tools_path/tools/esp-clang-libs/esp-21.1.3_20260408/esp-clang/esp-clang-libs.info"
libclang="$tools_path/tools/esp-clang-libs/esp-21.1.3_20260408/esp-clang/lib/libclang.dylib"
xtensa_gcc="$tools_path/tools/xtensa-esp-elf/esp-15.2.0_20251204/xtensa-esp-elf/bin/xtensa-esp-elf-gcc"
riscv_gcc="$tools_path/tools/riscv32-esp-elf/esp-15.2.0_20251204/riscv32-esp-elf/bin/riscv32-esp-elf-gcc"
cmake="$tools_path/tools/cmake/4.0.3/CMake.app/Contents/bin/cmake"
ninja="$tools_path/tools/ninja/1.12.1/ninja"
idf_python="$IDF_PYTHON_ENV_PATH/bin/python"
for executable in "$idf_python" "$clang" "$xtensa_gcc" "$riscv_gcc" "$cmake" "$ninja"; do
  [[ -f "$executable" && -x "$executable" ]] || {
    printf 'Pinned executable or library is missing: %s\n' "$executable" >&2
    exit 1
  }
done
[[ -f "$libclang" && -r "$libclang" ]] || {
  printf 'Pinned EspClang/libclang file is missing or unreadable: %s\n' "$libclang" >&2
  exit 1
}
canonical_path() {
  "$python_bin" -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$1"
}
clang=$(canonical_path "$clang")
libclang=$(canonical_path "$libclang")
xtensa_gcc=$(canonical_path "$xtensa_gcc")
riscv_gcc=$(canonical_path "$riscv_gcc")
cmake=$(canonical_path "$cmake")
ninja=$(canonical_path "$ninja")
[[ -f "$clang_libs_info" ]] || {
  printf 'Pinned EspClang/libclang package metadata is missing\n' >&2
  exit 1
}
for command_name in clang cmake ninja xtensa-esp-elf-gcc riscv32-esp-elf-gcc; do
  case "$command_name" in
    clang) expected_path="$clang" ;;
    cmake) expected_path="$cmake" ;;
    ninja) expected_path="$ninja" ;;
    xtensa-esp-elf-gcc) expected_path="$xtensa_gcc" ;;
    riscv32-esp-elf-gcc) expected_path="$riscv_gcc" ;;
  esac
  command_path=$(command -v "$command_name" || true)
  [[ -n "$command_path" && "$(canonical_path "$command_path")" == "$expected_path" ]] || {
    printf 'ESP-IDF export selected an unexpected %s executable\n' "$command_name" >&2
    exit 1
  }
done
expected_libclang_dir=$(canonical_path "$(dirname "$libclang")")
exported_libclang_dir=$(canonical_path "${ESP_CLANG_LIBS_PATH:-/missing}")
[[ "$exported_libclang_dir" == "$expected_libclang_dir" ]] || {
  printf 'ESP-IDF export selected an unexpected libclang directory\n' >&2
  exit 1
}
[[ "$("$cmake" --version | head -n 1)" == 'cmake version 4.0.3' ]] || {
  printf 'Unexpected CMake version\n' >&2
  exit 1
}
[[ "$("$ninja" --version)" == '1.12.1' ]] || {
  printf 'Unexpected Ninja version\n' >&2
  exit 1
}
[[ "$("$clang" --version | head -n 1)" == 'clang version 21.1.3'* ]] || {
  printf 'Unexpected EspClang version\n' >&2
  exit 1
}
[[ "$("$xtensa_gcc" --version)" == *'(crosstool-NG esp-15.2.0_20251204)'* ]] || {
  printf 'Unexpected Xtensa GCC version\n' >&2
  exit 1
}
[[ "$("$riscv_gcc" --version)" == *'(crosstool-NG esp-15.2.0_20251204)'* ]] || {
  printf 'Unexpected RISC-V GCC version\n' >&2
  exit 1
}
[[ "$("$python_bin" -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$clang_libs_info")" == 'esp-21.1.3_20260408' ]] || {
  printf 'Unexpected EspClang/libclang package version\n' >&2
  exit 1
}
"$idf_python" --version
"$python_bin" "$idf_tools" --non-interactive check
"$idf_path/tools/idf.py" --version

printf '##vso[task.setvariable variable=nimbleIdfRoot;isReadOnly=true]%s\n' "$idf_path"
printf '##vso[task.setvariable variable=nimbleIdfToolsRoot;isReadOnly=true]%s\n' "$tools_path"
printf '##vso[task.setvariable variable=nimbleIdfVenvRoot;isReadOnly=true]%s\n' "$IDF_PYTHON_ENV_PATH"
printf '##vso[task.setvariable variable=nimbleIdfVenvPython;isReadOnly=true]%s\n' "$idf_python"
printf '##vso[task.setvariable variable=nimbleEspClangPath;isReadOnly=true]%s\n' "$clang"
printf '##vso[task.setvariable variable=nimbleLibclangPath;isReadOnly=true]%s\n' "$libclang"
printf '##vso[task.setvariable variable=nimbleXtensaGccPath;isReadOnly=true]%s\n' "$xtensa_gcc"
printf '##vso[task.setvariable variable=nimbleRiscvGccPath;isReadOnly=true]%s\n' "$riscv_gcc"
printf '##vso[task.setvariable variable=nimbleIdfCmakePath;isReadOnly=true]%s\n' "$cmake"
printf '##vso[task.setvariable variable=nimbleIdfNinjaPath;isReadOnly=true]%s\n' "$ninja"
printf '##vso[task.setvariable variable=IDF_CCACHE_ENABLE;isReadOnly=true]0\n'
