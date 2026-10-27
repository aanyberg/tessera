# Tessera developer tasks. Run `just` to list.

set shell := ["zsh", "-cu"]

app_dir := "apps/macos"
xcframework := app_dir + "/TesseraCore.xcframework"

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

# Launch the built app
run: app
    open {{app_dir}}/DerivedData/Build/Products/Debug/Tessera.app

# Emit JSON Schema for the control protocol into docs/protocol (v1.x)
schema:
    @echo "schema generation lands with tessera-protocol schemars support"

clean:
    cargo clean
    rm -rf {{app_dir}}/DerivedData {{app_dir}}/Tessera.xcodeproj {{xcframework}}
