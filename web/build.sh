#!/usr/bin/env bash
# build.sh [--debug] [out-dir] — build the PWA into a static site (default:
# ../site), ready to serve or deploy: the app compiled for wasm32 and bound
# with wasm-bindgen, the page, the service worker (stamped with the build's
# version), the manifest, icons, fonts and their licences, and config.json
# from web/config.json (copy config.example.json and set the Dropbox app
# key; it is public under PKCE, but it is per deployment, so it is not
# committed).
#
# Needs the wasm32-unknown-unknown target (Arch: `pacman -S rust-wasm`) and
# a wasm-bindgen CLI of the SAME version as the wasm-bindgen crate in the
# lock (`cargo install wasm-bindgen-cli --version <it>`).
#
# Not under scripts/: ccebuild installs every crate's scripts/ into
# ~/.local/bin.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
crate=$(dirname "$here")
profile=release
if [[ ${1:-} == --debug ]]; then profile=debug; shift; fi
out=$(realpath -m "${1:-$crate/site}")

# Build from inside the crate: its .cargo/config.toml switches on web-sys's
# WebGPU bindings for the wasm target.
cd "$crate"
flags=()
[[ $profile == release ]] && flags+=(--release)
cargo build --lib --target wasm32-unknown-unknown "${flags[@]}"
target=${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')}
wasm="$target/wasm32-unknown-unknown/$profile/cce_vault_web.wasm"

want=$(grep -A1 '^name = "wasm-bindgen"$' "$crate/../Cargo.lock" 2>/dev/null | sed -n 's/version = "\(.*\)"/\1/p' | head -1 || true)
have=$(wasm-bindgen --version | awk '{print $2}')
if [[ -n $want && $want != "$have" ]]; then
    echo "build.sh: wasm-bindgen CLI is $have but the lock has $want" >&2
    exit 2
fi

rm -rf "$out"
mkdir -p "$out/fonts" "$out/icons"
wasm-bindgen --target web --no-typescript --out-dir "$out" "$wasm"
cp "$here/index.html" "$here/manifest.webmanifest" "$out/"
cp "$here"/icons/*.png "$out/icons/"
version=$(git -C "$crate" rev-parse --short HEAD 2>/dev/null || echo dev)-$(date +%s)
sed "s/__VERSION__/$version/" "$here/sw.js" > "$out/sw.js"
if [[ -f $here/config.json ]]; then
    cp "$here/config.json" "$out/config.json"
else
    echo "build.sh: no web/config.json — the app will ask for nothing but cannot log in" >&2
    echo '{}' > "$out/config.json"
fi

noto=/usr/share/fonts/noto
jb=/usr/share/fonts/TTF
cp "$noto"/NotoSans-{Regular,Bold,Italic,BoldItalic}.ttf "$noto"/NotoSerif-{Regular,Bold}.ttf "$out/fonts/"
cp "$jb"/JetBrainsMono-{Regular,Bold}.ttf "$out/fonts/"
for lic in noto-fonts ttf-jetbrains-mono; do
    for f in /usr/share/licenses/$lic/*; do
        [[ -f $f ]] && cp "$f" "$out/fonts/LICENSE-$lic-$(basename "$f")"
    done
done
echo "build.sh: site in $out ($(du -sh "$out" | cut -f1))"
