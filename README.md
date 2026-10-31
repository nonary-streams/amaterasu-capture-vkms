# Amaterasu VKMS capture provider

Experimental Linux capture provider for Amaterasu virtual displays. The provider discovers existing KWin virtual outputs and uses a separately authorized frame helper to obtain frames. Creating and removing virtual displays remains the display provider's responsibility.

This is experimental source; a build alone does not qualify a deployment. Native providers are trusted code; API grants do not sandbox them. Ordinary GPU snapshots do not prevent a compositor from reusing source storage during the copy. The required-lease composition below adds a driver read guard around that copy and still needs qualification with the installed driver and GPU.

## Source layout

- `capture/` contains the independently built provider. See its [detailed README](capture/README.md) for configuration and protocol behavior.
- `helper-wire/` builds the shared helper protocol library and the restricted `amaterasu-vkms-frame-helper` binary. The broker and installation/authorization policy remain separate; deploying the binary requires a compatible broker and explicit operator authorization.
- `capture/protocols/xdg-output-unstable-v1.xml` is third-party protocol material and retains its copyright and permission notice.

The capture and helper sources were extracted from Amaterasu commit `50570c3a757d9673eb7ada19a60124e2da6a2c74`. The initial extraction preserved the production source from that revision; one test queues its simulated export failure before waking the presenter to avoid a test race. The provider pins SDK revision `a3268d5db84bee3ae5181aae327d8e0bfab3fbb5`, a draft revision that includes the DMA-BUF lease API.

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

## Existing-driver capture (default)

The default build uses the released `WAIT_PRESENT` and `GET_FRAME` ioctls,
matching Vibeshine's presentation-driven DMA-BUF capture mechanism. It uses
helper protocol 1 and works with the existing authorized protocol-1 helper.
No driver upgrade or Vibeshine reinstall is required for this path.

With `gpu_snapshot:false`, the native table advertises capture v1.5: borrowed
DMA-BUF delivery and presentation queries. Descriptors remain valid during each
synchronous callback, but pinning an allocation does not prevent later compositor
writes. This path does not advertise immutable retention or zero copies across
the whole encoder pipeline. The encoder must finish using borrowed input before
its callback contract permits release.

The status response reports `frame_delivery: "borrowed_dmabuf"`. Building the
optional snapshot backend and selecting `gpu_snapshot:true` exposes the retained
frame interfaces using private copies. Source coherence during a GPU copy still
needs native qualification.

## Driver-backed GPU frame leases (experimental opt-in)

Build **both** capture and helper with `driver-read-leases` only when testing a
matching experimental driver. The direct export does not copy pixels. Helper protocol 2
requests the additive
`GET_FRAME_LEASED` ioctl from a matching driver. It exports read-only DMA-BUF
wrappers for the completed scanout, retaining a driver read lease. Before any
replacement commit applies its planes or signals its flip, it closes lease
acquisition and waits for existing leases. Acquisition and retirement are
serialized under the output's presentation lock. As with ordinary DRM scanout,
the producer must not modify a displayed buffer before its replacement completes.

The capture API lends that same GPU allocation. A consumer that reads after its
callback must retain the frame and release it with its GPU completion fence (or
`-1` only after its reads have finished). Accepted direct frames are removed from
the capture mailbox; otherwise a cached source lease would prevent the next
presentation. The driver wrapper remains alive through DMA-BUF importer
references, including across the helper/plugin descriptor handoff. Retaining a
frame applies backpressure to that output; there is no unsafe timeout that
permits producer reuse while a GPU may still read. A failed completion is
quarantined until the consumer process exits. `gpu_snapshot:true` remains an
optional GPU copy into private storage when decoupling is desired.

The default build keeps the existing driver ioctl. Only the opt-in helper
requires the new ioctl and rejects unleased responses; it does not fall back to an unprotected
scanout on an older driver. These changes have source/unit/build validation;
GPU import, display pacing and shutdown behavior still require hardware testing.

## Required leases with bounded source retention

For streaming, build capture with
`driver-read-leases,linux-kwin-geometry,linux-gpu-snapshot` and the helper with
`driver-read-leases`. Set `gpu_snapshot:true` and `require_lease:true` in capture,
and `require_frame_lease:true` in the video component. Capture then holds the
driver guard only until its GPU copy and temporary source import have completed.
The encoder retains the independent immutable snapshot, so its queue does not
hold the compositor's scanout. This performs one GPU capture copy with no CPU
pixel readback.

Both components reject a borrowed or CPU fallback when leases are required.
Missing driver support, the wrong helper protocol, unavailable GPU snapshot
support or an encoder without the lease interface fails explicitly. This does
not eliminate stalls during a stuck GPU operation or establish forced-reboot
safety for arbitrary retained direct imports.
