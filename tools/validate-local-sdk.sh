#!/bin/sh
# Validate the portable pinned provider against an unpublished reviewed SDK.
set -eu
if [ "$#" -ne 1 ]; then
    echo "Usage: $0 /absolute/path/to/reviewed-sdk.git" >&2
    exit 2
fi
case "$1" in /*) ;; *) echo "SDK repository must be absolute" >&2; exit 2;; esac
sdk_repository=$(cd "$1" && pwd)
provider_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
sdk_revision=a5ed264bcb06880204f2ee24bb3e7ede425fafff
actual_revision=$(git -C "$sdk_repository" rev-parse "$sdk_revision^{commit}")
if [ "$actual_revision" != "$sdk_revision" ]; then
    echo "Reviewed SDK revision is unavailable" >&2
    exit 1
fi
export CARGO_HOME="$provider_root/target/local-sdk-cargo"
export CARGO_TARGET_DIR="$provider_root/target/local-sdk"
export CARGO_BUILD_JOBS=2 CARGO_NET_GIT_FETCH_WITH_CLI=true
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0="url.file://$sdk_repository.insteadOf"
export GIT_CONFIG_VALUE_0=https://github.com/nonary-streams/amaterasu-sdk
cargo test --locked --manifest-path "$provider_root/capture/Cargo.toml" --lib
cargo test --locked --manifest-path "$provider_root/capture/Cargo.toml" --lib --all-features
cargo clippy --locked --manifest-path "$provider_root/capture/Cargo.toml" --all-targets --all-features -- -D warnings
