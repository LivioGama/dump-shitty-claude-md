#!/usr/bin/env bash
# Build release binaries for every supported npm platform into bin/<platform>-<arch>/.
# Host target builds with plain cargo; other darwin targets via `rustup run`;
# foreign OS targets need `cross` (docker). Missing toolchains are skipped.
set -uo pipefail
cd "$(dirname "$0")/.."

HOST=$(rustc -vV | awk '/^host:/{print $2}')

# target triple → npm <platform>-<arch>
TARGETS=(
  "aarch64-apple-darwin:darwin-arm64"
  "x86_64-apple-darwin:darwin-x64"
  "x86_64-unknown-linux-gnu:linux-x64"
  "aarch64-unknown-linux-gnu:linux-arm64"
  "x86_64-pc-windows-msvc:win32-x64"
)

for entry in "${TARGETS[@]}"; do
  triple="${entry%%:*}"; plat="${entry##*:}"
  exe="dump-shitty-claude-md"; [[ $triple == *windows* ]] && exe="$exe.exe"

  if [[ $triple == "$HOST" ]]; then
    cmd=(cargo build --release)
    out="target/release/$exe"
  elif [[ $triple == *apple-darwin* ]] && command -v rustup >/dev/null \
       && rustup target list --installed | grep -qx "$triple"; then
    # macOS clang links both arches. NB: `rustup run` doesn't shadow a Homebrew
    # cargo/rustc in PATH — prepend the toolchain bin dir explicitly.
    tbin="$HOME/.rustup/toolchains/stable-$HOST/bin"
    cmd=(env "PATH=$tbin:$PATH" cargo build --release --target "$triple")
    out="target/$triple/release/$exe"
  elif command -v cross >/dev/null; then
    cmd=(cross build --release --target "$triple")
    out="target/$triple/release/$exe"
  else
    echo "skip $triple (install 'cross' + docker for foreign targets)"
    continue
  fi

  echo "build $triple → bin/$plat/"
  "${cmd[@]}" || { echo "FAIL $triple"; continue; }
  mkdir -p "bin/$plat"
  cp "$out" "bin/$plat/"
done
echo "done. bin/: $(ls bin/ | grep -v '\.js$' | tr '\n' ' ')"
