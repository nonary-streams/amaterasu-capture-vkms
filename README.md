# Amaterasu VKMS capture provider

Experimental Linux capture provider for Amaterasu virtual displays. The provider discovers existing KWin virtual outputs and uses a separately authorized frame helper to obtain frames. Creating and removing virtual displays remains the display provider's responsibility.

This is draft source: it is not an installed component or a hardware qualification. Native providers are trusted code; API grants do not sandbox them. The GPU snapshot path does not yet establish that a compositor cannot reuse its source allocation while the copy is in progress. Frame coherence requires a producer-lifetime guarantee or platform-specific qualification.

## Source layout

- `capture/` contains the independently built provider. See its [detailed README](capture/README.md) for configuration and protocol behavior.
- `helper-wire/` builds the shared helper protocol library and the restricted `amaterasu-vkms-frame-helper` binary. The broker and installation/authorization policy remain separate; deploying the binary requires a compatible broker and explicit operator authorization.
- `capture/protocols/xdg-output-unstable-v1.xml` is third-party protocol material and retains its copyright and permission notice.

The capture and helper sources were extracted from Amaterasu commit `50570c3a757d9673eb7ada19a60124e2da6a2c74`. The extraction preserves the production source from that revision; one test queues its simulated export failure before waking the presenter to avoid a test race. The provider pins SDK revision `a3268d5db84bee3ae5181aae327d8e0bfab3fbb5`, a draft revision that includes the DMA-BUF lease API.

## Local validation

From `capture/`, with Rust 1.89 or newer, network access to fetch the pinned SDK and crates, and the Linux Wayland/EGL development dependencies:

```sh
cargo test --locked --all-features
cargo fmt --all --check
cargo clippy --locked --all-features --all-targets -- -D warnings
```

These tests do not approve or run the privileged helper, capture desktop frames, exercise a GPU encoder, or establish client playback.

From `helper-wire/`, validate the helper and wire protocol independently:

```sh
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
```

The default tests run an unprivileged helper; the ignored installed-helper test requires an operator-installed, authorized helper and the vibeshine_drm driver. Building or testing does not install the binary or grant capabilities.
