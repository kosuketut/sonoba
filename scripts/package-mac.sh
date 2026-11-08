#!/bin/sh
# Builds dist/sonoba-mac-<version>.zip: Sonoba.app with the official llama.cpp engine for Apple
# silicon inside it, and every license text. The model is not in the package: users download it
# from the settings window.
#
# The app is signed ad hoc, not with a Developer ID, so the first start needs Open in Finder's
# menu, or "Open Anyway" in System Settings. See the README.
set -eu
tag=b12144
asset="llama-$tag-bin-macos-arm64.tar.gz"
sha256=2045e1d2dd7f06beed9a9a7594db1bb1c42e5b92bffcb3bd503679c8d7b6c8a9

root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
name="sonoba-mac-$version"
dist="$root/dist"
stage="$dist/$name"
app="$stage/Sonoba.app"

cargo build --release --target aarch64-apple-darwin --manifest-path "$root/Cargo.toml"

rm -rf "$stage"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/engine"
cp "$root/target/aarch64-apple-darwin/release/sonoba" "$app/Contents/MacOS/sonoba"

# The engine: llama-server and the libraries it links, which it finds beside it through
# @loader_path. The other tools of the release, and their libraries, stay out.
tarball="$dist/$asset"
[ -f "$tarball" ] || curl -sSfL -o "$tarball" "https://github.com/ggml-org/llama.cpp/releases/download/$tag/$asset"
if [ "$(shasum -a 256 "$tarball" | cut -d' ' -f1)" != "$sha256" ]; then
    rm -f "$tarball"
    echo "$asset does not match its expected hash" >&2
    exit 1
fi
unpacked="$dist/llama-$tag"
rm -rf "$unpacked"
tar xzf "$tarball" -C "$dist"
cp "$unpacked/llama-server" "$app/Contents/Resources/engine/"
for lib in ggml ggml-base ggml-blas ggml-cpu ggml-metal ggml-rpc llama llama-common mtmd; do
    cp -L "$unpacked/lib$lib.0.dylib" "$app/Contents/Resources/engine/" # the real file, under the name it is linked by
done
cp "$unpacked/libllama-server-impl.dylib" "$app/Contents/Resources/engine/"
rm -rf "$unpacked"

# The icon, from the one Windows uses.
iconset="$dist/AppIcon.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"
for size in 16 32 64 128 256; do
    sips -s format png -z $size $size "$root/app.ico" --out "$iconset/icon_${size}x${size}.png" >/dev/null
done
for size in 16 32 128; do
    cp "$iconset/icon_$((size * 2))x$((size * 2)).png" "$iconset/icon_${size}x${size}@2x.png"
done
rm "$iconset/icon_64x64.png"
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset"

# LSUIElement keeps the app out of the Dock: it lives in the menu bar. ScreenCaptureKit needs
# macOS 12.3.
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Sonoba</string>
  <key>CFBundleDisplayName</key><string>Sonoba</string>
  <key>CFBundleIdentifier</key><string>jp.sonoba.app</string>
  <key>CFBundleExecutable</key><string>sonoba</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.3</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSScreenCaptureUsageDescription</key><string>Sonoba reads the text in the region you select, to translate it.</string>
</dict>
</plist>
EOF

# The license texts. The Rust crates differ from the Windows build's, so their list is made for
# this target; the other texts are shared.
mkdir -p "$stage/licenses"
for file in "$root"/licenses/*.txt; do
    case "$(basename "$file")" in
        rust-crates.txt | webview2-rs.txt | WebView2-SDK.txt) ;;
        *) cp "$file" "$stage/licenses/" ;;
    esac
done
crates="$dist/crates.txt"
cargo tree --manifest-path "$root/Cargo.toml" -e normal --target aarch64-apple-darwin --prefix none --format '{p}|{l}' | sort -u > "$crates"
python3 - "$crates" "$stage/licenses/rust-crates.txt" <<'EOF'
# Writes the license text of every crate linked in, crates that share a text listed together.
import pathlib, re, sys
registry = next((pathlib.Path.home() / ".cargo/registry/src").iterdir())
texts, bare = {}, []
for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
    found = re.match(r"^(\S+) v(\S+).*\|(.*)$", line.strip())
    if not found or found[1] == "sonoba":
        continue
    name, version, license = found[1], found[2], re.sub(r" \(\*\)$", "", found[3])
    crate = f"{name} {version} ({license})"
    folder = registry / f"{name}-{version}"
    files = [f for f in folder.iterdir() if f.is_file() and re.match(r"^(licen[sc]e|copying|notice|unlicense)", f.name, re.I)] if folder.is_dir() else []
    if not files and crate not in bare:
        bare.append(crate)
    for file in files:
        texts.setdefault(file.read_text(errors="replace").replace("\r\n", "\n").strip(), set()).add(crate)
out = ["Rust crates linked into Sonoba for macOS, with their license texts.", "Generated by scripts/package-mac.sh.", ""]
for text, crates in sorted(texts.items(), key=lambda entry: sorted(entry[1])[0]):
    out += ["=" * 78, *sorted(crates), "-" * 78, text, ""]
if bare:
    out += ["=" * 78, "These crates ship no license file. See rten.txt and objc2.txt beside this file.", *bare]
pathlib.Path(sys.argv[2]).write_text("\n".join(out) + "\n")
print(f"{len(texts)} distinct license texts, {len(bare)} crates without a file: {', '.join(bare)}")
EOF
rm "$crates"
cp -R "$stage/licenses" "$app/Contents/Resources/"

# Inside out: the engine's libraries and the engine first, then the app around them.
codesign --force --sign - "$app/Contents/Resources/engine/"*.dylib "$app/Contents/Resources/engine/llama-server" 2>/dev/null
codesign --force --sign - "$app" 2>/dev/null
codesign --verify --strict "$app"

cp "$root/README.md" "$root/README.en.md" "$root/LICENSE" "$root/NOTICE.md" "$stage/"
rm -f "$dist/$name.zip"
(cd "$stage" && ditto -c -k --norsrc --noextattr --noacl . "$dist/$name.zip")
# The README links releases/latest/download/sonoba-mac.zip, so every release also carries this copy.
cp "$dist/$name.zip" "$dist/sonoba-mac.zip"
echo "dist/$name.zip  $(($(stat -f %z "$dist/$name.zip") / 1048576)) MB (and dist/sonoba-mac.zip for the release)"
