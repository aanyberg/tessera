# Tessera developer tasks. Run `just` to list.

set shell := ["zsh", "-cu"]

app_dir := "apps/macos"
xcframework := app_dir + "/TesseraCore.xcframework"
build_dir := app_dir + "/build"
archive_path := build_dir + "/Tessera.xcarchive"

default:
    @just --list

# Format, lint, test, license check
check: fmt-check clippy test deny

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo nextest run --workspace

deny:
    cargo deny check

# Regenerate docs/config.md from the tessera-config types (doc comments and defaults)
config-docs:
    cargo xtask config-docs

# Fail with a unified diff if docs/config.md is out of date
config-docs-check:
    cargo xtask config-docs --check

# Build the Rust static library for both macOS architectures (release)
core-release:
    cargo build --release -p tessera-ffi --target aarch64-apple-darwin
    cargo build --release -p tessera-ffi --target x86_64-apple-darwin

# Produce a universal TesseraCore.xcframework with the generated header
xcframework: core-release
    rm -rf {{xcframework}} target/xcframework
    mkdir -p target/xcframework/include
    cp target/include/TesseraCore.h target/xcframework/include/
    printf 'module TesseraCore {\n    header "TesseraCore.h"\n    export *\n}\n' > target/xcframework/include/module.modulemap
    lipo -create \
        target/aarch64-apple-darwin/release/libtessera_ffi.a \
        target/x86_64-apple-darwin/release/libtessera_ffi.a \
        -output target/xcframework/libtessera_ffi.a
    xcodebuild -create-xcframework \
        -library target/xcframework/libtessera_ffi.a \
        -headers target/xcframework/include \
        -output {{xcframework}}

# Generate the Xcode project from project.yml
xcodeproj:
    cd {{app_dir}} && xcodegen generate

# Build the macOS app (debug)
app: xcframework xcodeproj
    xcodebuild -project {{app_dir}}/Tessera.xcodeproj -scheme Tessera -configuration Debug \
        -derivedDataPath {{app_dir}}/DerivedData build | tail -5

# Run the TesseraKit wrapper's XCTest suite (headless, Thread Sanitizer); full log in apps/macos/build/app-test.log
app-test: xcframework xcodeproj
    mkdir -p {{quote(build_dir)}}
    set -o pipefail; xcodebuild test -project {{quote(app_dir + "/Tessera.xcodeproj")}} -scheme TesseraKit -enableThreadSanitizer YES \
        -destination 'platform=macOS' -derivedDataPath {{quote(app_dir + "/DerivedData")}} 2>&1 | tee {{quote(build_dir + "/app-test.log")}} | tail -5 \
        || { echo "xcodebuild test failed; errors from {{build_dir}}/app-test.log:" >&2; grep -E 'error:|failed|ThreadSanitizer' {{quote(build_dir + "/app-test.log")}} >&2; exit 1 }

# Launch the built app
run: app
    open {{app_dir}}/DerivedData/Build/Products/Debug/Tessera.app

terminfo := "shell-integration/terminfo/install.sh"

# Compile the tessera terminfo entry into a temp dir and list its extended caps
terminfo-check:
    {{terminfo}} --check

# Install the tessera terminfo entry into ~/.terminfo
terminfo-install:
    {{terminfo}}

# Install the tessera terminfo entry on a remote host over ssh
terminfo-ssh host:
    {{terminfo}} --ssh {{host}}

# Release: archive the app (Release, universal, ad-hoc signed); version = CFBundleShortVersionString (numeric), build = CFBundleVersion
archive version="0.1.0" build="1": xcodeproj
    test -d {{quote(xcframework)}} || { echo "missing {{xcframework}}: run 'just xcframework' first" >&2; exit 1 }
    rm -rf {{quote(archive_path)}}
    mkdir -p {{quote(build_dir)}}
    set -o pipefail; xcodebuild archive -project {{quote(app_dir + "/Tessera.xcodeproj")}} -scheme Tessera -configuration Release \
        -destination "generic/platform=macOS" \
        -archivePath {{quote(archive_path)}} -derivedDataPath {{quote(app_dir + "/DerivedData")}} \
        MARKETING_VERSION={{quote(version)}} CURRENT_PROJECT_VERSION={{quote(build)}} \
        CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM="" 2>&1 | tee {{quote(build_dir + "/archive.log")}} | tail -5 \
        || { echo "xcodebuild archive failed; errors from {{build_dir}}/archive.log:" >&2; grep -E 'error:|\*\* ARCHIVE FAILED' {{quote(build_dir + "/archive.log")}} >&2; exit 1 }
    lipo -archs {{quote(archive_path + "/Products/Applications/Tessera.app/Contents/MacOS/Tessera")}}

# Release: zip the archived app as Tessera-<tag>-macos-universal.zip (ditto keeps bundle metadata; no __MACOSX AppleDouble tree)
package tag="v0.0.0":
    rm -f {{quote(build_dir + "/Tessera-" + tag + "-macos-universal.zip")}}
    ditto -c -k --keepParent --norsrc \
        {{quote(archive_path + "/Products/Applications/Tessera.app")}} \
        {{quote(build_dir + "/Tessera-" + tag + "-macos-universal.zip")}}
    ls -l {{quote(build_dir + "/Tessera-" + tag + "-macos-universal.zip")}}

# Release: preview the git-cliff notes for the unreleased commits *before* tagging (e.g. `just changelog v0.1.0-rc.1`); once HEAD is tagged use `just release-notes <tag>`
changelog tag="":
    git-cliff --unreleased {{ if tag != "" { "--tag " + quote(tag) } else { "" } }} --strip all

# Release: render the notes the workflow attaches for a tag that points at HEAD
release-notes tag:
    scripts/release-notes.sh {{quote(tag)}}

# Emit JSON Schema for the control protocol into docs/protocol (v1.x)
schema:
    @echo "schema generation lands with tessera-protocol schemars support"

clean:
    cargo clean
    rm -rf {{app_dir}}/DerivedData {{app_dir}}/Tessera.xcodeproj {{xcframework}} {{build_dir}}
