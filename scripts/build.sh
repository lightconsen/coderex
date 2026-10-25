#!/usr/bin/env bash
# Build CodeRex locally.
#
# Usage:
#   scripts/build.sh                       # release build of the coderex binary
#   scripts/build.sh --debug               # fast debug build
#   scripts/build.sh --workspace           # build every workspace crate
#   scripts/build.sh --clean               # clean first, then build
#   scripts/build.sh --run -- --version    # build, then run with arguments
#
# Options:
#   -d, --debug       use the dev profile (fast to compile, slow to run)
#   -w, --workspace   build the whole workspace instead of just the coderex bin
#   -c, --clean       run `cargo clean` before building
#   -j, --jobs N      parallel build jobs (passed to cargo)
#       --run         run the built binary; arguments after `--` go to it
#   -h, --help        show this help
#
# The script checks the toolchain (Rust 1.97 or newer), a C compiler, and on
# Linux pkg-config, the dbus development files and lld (the linker configured
# in `.cargo/config.toml`). It never modifies the repository.

set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

usage() {
  sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '%s\n' "$*"
}

profile=release
workspace=0
clean=0
run=0
jobs=""
run_args=()

while [ $# -gt 0 ]; do
  case "$1" in
    -d | --debug) profile=debug ;;
    -w | --workspace) workspace=1 ;;
    -c | --clean) clean=1 ;;
    --run) run=1 ;;
    -j | --jobs)
      [ $# -ge 2 ] || die "--jobs needs a value"
      jobs=$2
      shift
      ;;
    --jobs=*) jobs=${1#*=} ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      run_args=("$@")
      break
      ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
  shift
done

command -v cargo >/dev/null 2>&1 ||
  die "cargo is not installed; install Rust from https://rustup.rs"
command -v rustc >/dev/null 2>&1 ||
  die "rustc is not installed; install Rust from https://rustup.rs"

# The workspace MSRV is 1.97 (wasmtime 50 requires it).
version=$(rustc --version | awk '{print $2}')
major=${version%%.*}
rest=${version#*.}
minor=${rest%%.*}
if [ "$major" -lt 1 ] || { [ "$major" -eq 1 ] && [ "$minor" -lt 97 ]; }; then
  die "Rust $version is too old; this workspace needs 1.97 or newer (rustup update stable)"
fi

case "$(uname -s)" in
  Linux)
    command -v cc >/dev/null 2>&1 ||
      die "no C compiler found; install gcc or clang (QuickJS, ring and wasmtime are built from source)"
    command -v pkg-config >/dev/null 2>&1 ||
      die "pkg-config is missing; libdbus-sys needs it (install pkg-config)"
    pkg-config --exists dbus-1 ||
      die "the dbus development files are missing; install libdbus-1-dev (Debian/Ubuntu) or dbus-devel (Fedora)"
    if ! command -v ld.lld >/dev/null 2>&1 && ! command -v lld >/dev/null 2>&1; then
      die "lld is missing; .cargo/config.toml links x86_64 Linux with -fuse-ld=lld (install lld)"
    fi
    ;;
  Darwin)
    command -v cc >/dev/null 2>&1 ||
      die "no C compiler found; install the Xcode command line tools"
    ;;
  MINGW* | MSYS* | CYGWIN*)
    note "note: on Windows, build from a shell with the MSVC toolchain available"
    ;;
esac

if [ "$clean" = 1 ]; then
  note "cleaning the target directory..."
  cargo clean
fi

cargo_args=()
if [ "$profile" = release ]; then
  cargo_args+=(--release)
fi
if [ "$workspace" = 1 ]; then
  :
else
  cargo_args+=(-p coderex-cli --bin coderex)
fi
if [ -n "$jobs" ]; then
  cargo_args+=(-j "$jobs")
fi

if [ "$workspace" = 1 ]; then
  note "building the workspace ($profile)..."
else
  note "building the coderex binary ($profile)..."
fi
note "the first build compiles QuickJS, wasmtime and the workspace; expect it to take several minutes"

cargo build "${cargo_args[@]}"

target_dir=${CARGO_TARGET_DIR:-$root/target}
binary="$target_dir/$profile/coderex"
if [ ! -x "$binary" ]; then
  if [ -x "$binary.exe" ]; then
    binary="$binary.exe"
  else
    die "the build succeeded but $binary was not found"
  fi
fi
note "built: $binary"

if [ "$run" = 1 ]; then
  if [ ${#run_args[@]} -gt 0 ]; then
    note "running: $binary ${run_args[*]}"
    exec "$binary" "${run_args[@]}"
  fi
  note "running: $binary"
  exec "$binary"
fi
