# tuiblick — `just` recipes. Run `just --list` to see them.
# `build` = debug binary in target/debug. `install` = release build + install to cargo bin.

default: build

# Debug build (binary: ./target/debug/tuiblick)
build:
    cargo build

# Release build (binary: ./target/release/tuiblick)
build-release:
    cargo build --release

# Release build + install to cargo bin (~/.cargo/bin/tuiblick)
install: build-release
    cargo install --path .

# Full gate: fmt + lint + tests (must pass before commits)
check:
    cargo fmt --check
    cargo clippy -- --deny warnings
    cargo test
