# vibeshine_drm DMA-BUF capture

This independently built trusted native library provides control and native
`amaterasu.capture-vkms/1` for the owned virtual outputs that the
`vibeshine_drm` kernel driver exposes as `Virtual-N` connectors. Frames are the
driver's latest completed primary-plane scanout of the output's CRTC, delivered
as DMA-BUF descriptors. Optional private GPU snapshots supply immutable frame
leases without a CPU copy. Native API grants do not
sandbox native code. The provider exists only on Linux; other targets build an
empty library.

The driver exports frames only to callers with `CAP_SYS_ADMIN`. The host
process never receives that capability. A small dedicated helper,
`amaterasu-vkms-frame-helper` (crate `crates/vkms-frame-helper`), holds it and
does nothing else.

## Package-approved authority

New capture packages select `authority_channel:true` instead of `frame_helper`.
The protected launcher supplies `authority_channel_fd` to the approved component
only after verifying the package and machine-protected approval. This descriptor
is a private control channel, not an executable path or caller-provided identity.
Each stream obtains an independent broker-proxied helper session. Both control
and session sockets must have a root peer. The broker checks the exact package
approval and resource scope on every helper request and owns revocation.

The helper's `--approved-service` mode requires all real, effective and saved
UID/GID values to be root and a root peer at fd 3. It is executed only by the
protected broker from its root-only installed artifact. The user-process module
is never elevated. Missing, denied or revoked authority fails without trying a
legacy helper. Approvals, protected installation and active deployment are
separate states; selecting this setting does not install the broker or approve
a request. In-process native modules remain trusted code.

```json
{"enabled":true,"capture_principals":["amaterasu.video"],"authority_channel":true,"gpu_snapshot":true}
```

`authority_channel_fd` is reserved for host injection and must not be placed in
package-controlled policy. Simultaneous authority mode and a legacy helper path
are rejected. No injected channel yields `authority_unavailable`; an explicit
broker denial yields `authority_denied`.

## Installing the legacy helper

Install the helper as a root-owned file that group and others cannot write,
for example `/usr/libexec/amaterasu/amaterasu-vkms-frame-helper` with mode
0755, then grant it exactly one permitted file capability:

```sh
sudo setcap cap_sys_admin+p /usr/libexec/amaterasu/amaterasu-vkms-frame-helper
```

Grant `+p` only, never `+ep` or other capabilities. The helper refuses to run if
it holds any other capability after dropping, or if any real, effective or saved
user or group id is root. The host service must not set `NoNewPrivileges=yes`,
must keep `CAP_SYS_ADMIN` in its bounding set, and the file system holding the
helper must not be mounted `nosuid`; otherwise the kernel does not apply the
file capability and the provider reports `permission_denied`.

## Settings

```json
{"enabled":false,"capture_principals":[],"frame_helper":null}
```

An administrator-controlled startup policy enables capture, allowlists
authenticated immediate callers (at most 16) in `capture_principals`, and sets
`frame_helper` to the helper's absolute path. Each time the helper is used the
module opens that path and checks the opened file: a regular executable file
owned by root, not writable by group or others, and without setuid or setgid
bits. It then executes that same opened inode, so replacing the path afterward
cannot substitute another program.

## Control actions

The actions and reply shapes follow `amaterasu.capture-pipewire/1`. Failures
reply `{"ok":false,"error":{"code":"...","message":"..."}}`.

- `{"action":"status"}` reports `enabled`, `native_supported`,
  `frame_helper_configured`, the `availability` from the latest enumeration,
  the caller's `streams` (with `frames_delivered` and any `fault`), and lease
  counters `opened`, `closed`, `active`.
- `{"action":"enumerate"}` requires a capture principal. Optional `offset`
  must be zero and `limit` is 1 through 256. It returns every `Virtual-N`
  connector on every vibeshine_drm card as `outputs` entries with `id`
  (`kwin:Virtual-N`), `device_name` (`Virtual-N`), `attached` (connected with an
  active CRTC and mode), `card`, `connector_id`, `connection`, `crtc_id`,
  `width`, `height`, `refresh_hz`, `format:"dmabuf"`, `geometry_supported`
  and, when the compositor layout agrees, `desktop_geometry`.
  `availability` has `available`, `reason` and an operator `message`; `checks`
  reports the helper and driver separately. Reasons are
  `frame_helper_unavailable` (not configured, unusable file, or the helper
  refused to start), `permission_denied` (the helper lacks `CAP_SYS_ADMIN`; the
  message names the `setcap` command) and `driver_unavailable` (no readable
  vibeshine_drm card). Enumeration starts the helper once to check it.
- `{"action":"open","output":"kwin:Virtual-9","operation_id":"video-1"}`
  requires a capture principal and an operation id. `constant_rate:true` fails
  with `unsupported_constant_rate`. The output is resolved to its current CRTC,
  a helper is started for the stream, opens the card and exports one frame to
  prove the path; only then is the stream's presentation thread started and
  the stream reported. The reply carries
  `stream`, `output`, `operation_id`, `card`, `connector_id`, `crtc_id`, the
  mode, `frame_transport:"dmabuf"`, `geometry_supported` and, when supported,
  `desktop_geometry` (see Geometry). Replaying the same operation id returns the same stream;
  a different output with that id is `operation_conflict`. Other definite
  refusals include `invalid_named_output`, `output_not_found`,
  `output_disconnected`, `output_inactive`, `output_ambiguous`,
  `output_unavailable`, `output_geometry_unavailable`, `frame_helper_failed`
  and `resource_exhausted` (at most
  4 open streams and 128 per module instance).
- `{"action":"lookup","operation_id":"video-1"}` recovers the caller's stream.
- `{"action":"close","stream":123}` is idempotent for its owner. It stops
  admitting native reads at once, stops the stream's presentation thread and
  ends its helper; a read admitted before close finishes with its descriptors
  still valid.

## Output resolution

`kwin:Virtual-N` names the DRM connector `Virtual-N` (connector type Virtual,
type index N). N is whatever the driver assigned and changes when the module is
reloaded, so any N >= 1 is accepted. Resolution reads only entry names and
`cardN/dev` under `/sys/class/drm` to find cards with a `cardN-Virtual-N`
connector, then opens `/dev/dri/cardN` read-only, confirms the device number
and that `DRM_IOCTL_VERSION` names `vibeshine_drm`, and issues read-only
GETRESOURCES, GETCONNECTOR, GETENCODER and GETCRTC queries to find the
connector's encoder, CRTC and mode. The descriptor is closed afterward. If the
kernel made it DRM master because no compositor held the device, master is
dropped immediately so the compositor can still acquire it; no other DRM state
is touched. Open streams re-resolve once per second, and on the next host tick
when the presentation thread reports the CRTC is gone: a CRTC or mode change is followed, while a
disconnected, inactive or replaced connector faults the stream.

## Geometry

Build with the `linux-kwin-geometry` feature to report desktop geometry. It
links a minimal unprivileged Wayland client (`wayland-client` through
pkg-config, plus `xdg-output-unstable-v1` generated by `wayland-scanner` from
`third-party/protocols`). It binds only `wl_registry`, `wl_output` (up to v4)
and `zxdg_output_manager_v1`; it never binds the screencast protocol. Without
the feature, or on other targets, geometry is unsupported.

The semantics match capture-pipewire's `kwin:Virtual-N` geometry exactly. The
output rectangle is the compositor's xdg-output logical position and size of
the `wl_output` named `Virtual-N`, so a scaled output reports its logical size
(xdg-output, not the integer `wl_output.scale`). The virtual desktop is the
bounding box of every live output. An output without a positive logical size
up to 32768, or a desktop wider or taller than 65536, makes the layout
invalid. On this host the owned outputs are appended to the right of the
user's desktop, so for example a 3024x1890 primary at scale 2.5 (1210x756
logical) followed by a 1920x1080 owned output yields output `{1210,0,1920,1080}`
in desktop `{0,0,3130,1080}`.

The DRM CRTC mode is a cross-check: the compositor's current `wl_output` pixel
mode must equal the scanout mode that produces the frames.

- Without a reachable Wayland display at open, the open reply has
  `geometry_supported:false` and no `desktop_geometry`, and the geometry
  callback returns `MEDIA_UNSUPPORTED` for that stream's lifetime.
- With a display, open requires a consistent layout now; a missing output or a
  mode mismatch refuses the open with `output_geometry_unavailable` rather than
  reporting a wrong rectangle.
- During a stream the module polls the Wayland connection without blocking on
  every tick and recomputes geometry on each `wl_output` done event, output
  addition or removal, and on the 1 s re-resolve. While the layout is unknown
  or disagrees with the scanout mode the geometry callback returns
  `MEDIA_BUSY`; afterwards it returns the new rectangles, which consumers see
  as a geometry change.

A lost compositor connection is retried at most once per second.

## Native frames

The native table is `CaptureApiV1_6` (major 1, minor 6), a strict superset of
`CaptureApiV1_5`; older DMA-BUF consumers bind and read the same prefix.
`read_frame`, `read_frame_with_freshness` and
`read_frame_with_capture_timestamp` return `MEDIA_UNSUPPORTED`.

Frames are pushed by presentation, not sampled. Each open stream has one
presentation thread that alone owns the stream's helper. It asks the helper to
WAIT (bounded to 100 ms) for a sequence newer than the last one it saw; as soon
as the driver completes a newer presentation it exports it with FRAME and
stores it as the stream's newest frame, replacing the previous one. A timeout
just waits again, so frames arrive at the compositor's presentation rate,
including variable refresh, and nothing is exported while the output is idle.
FRAME always returns the newest completed scanout, so presentations completed
during an export are coalesced into the next frame.

`read_dmabuf_frame` never talks to the helper. It lends the stored newest frame
to the sink exactly once; that frame is the buffer currently scanned out, so
lending it again for a repeat is valid:

- fourcc, modifier, plane layout and per-plane acquire fences are the driver's.
  `color` is `DMABUF_COLOR_SRGB`.
- `sequence` is the driver presentation sequence. If the output moves to
  another CRTC, sequences continue after the last stored one, so a stream's
  sequences never decrease. Freshness is `CAPTURE_FRAME_FRESH` when the stored
  sequence is newer than the last one the sink accepted on this stream,
  otherwise `CAPTURE_FRAME_CACHED_REUSE`; a rejected fresh frame stays fresh on
  the next read.
- `timestamp_us` is the driver's CLOCK_MONOTONIC presentation time in
  microseconds since the stream opened. A presentation completed before open
  maps to zero, and timestamps never decrease.
- The DMA-BUF and fence descriptors are borrowed for the callback only and
  stay owned by the stream. A frame replaced while a read borrows it is closed
  when that read returns; otherwise a replaced frame is closed at once and the
  newest one when the stream ends. A consumer that keeps a buffer must
  duplicate its descriptor first. An open DMA-BUF pins the allocation but does
  not stop the compositor from rendering into it again.

`latest_presentation` writes the stored newest frame's `sequence` and
`timestamp_us` (the same values its `DmaBufFrame` carries), or zeros before the
first frame. It never calls the helper. A consumer polls it each tick and reads
only when the sequence advances.

No frame yet is `MEDIA_PENDING`; a frame the module cannot describe (larger
than the DMA-BUF limits) is `MEDIA_UNSUPPORTED`; a busy controller or a second
concurrent read of the same stream is `MEDIA_BUSY`; a closed or faulted-output
stream is `MEDIA_CLOSED`; a failed helper or permission refusal is
`MEDIA_BACKEND_ERROR` for both reads and presentation queries.

### Immutable GPU frame leases

Build with `--features linux-kwin-geometry,linux-gpu-snapshot` and set
`gpu_snapshot:true`. Snapshot support requires GBM, EGL, GLES and libdrm
development libraries, plus access to a compatible render node at runtime.
The presentation thread waits for all input acquire fences, then renders into
a fresh private GBM allocation and calls `glFinish` before exporting it. This
provider never writes that allocation again. RGB8 input uses XR24 storage;
XR30, AR30, XB30 and AB30 input uses XR30 storage, a high-precision sampler and
a framebuffer with at least ten bits per RGB component. Unsupported import or
render targets fail without reducing precision. No CPU readback occurs.

`read_dmabuf_frame_leased` supports only these completed private snapshots.
Ordinary mutable scanout exports return `MEDIA_UNSUPPORTED` from the leased
read. Retaining pins immutable pixels and descriptors even after frame
replacement or stream close. The consumer must retain the native binding until
it releases every frame. A global limit of eight retained references applies
across frames, streams and stream restarts; a full limit returns `MEDIA_BUSY`
and acquires nothing.

Release consumes exactly one retained reference and its completion fence.
`-1` means consumer GPU work has finished. Other completion fences must be real
Linux sync_file descriptors and are waited for up to one second. An invalid,
failed or stalled completion fence permanently quarantines its allocation and
capacity slot until the process ends, preventing premature reuse. This bounded
failure behavior can make later retains busy; the usual synchronous encoder
path completes its read before release and uses `-1`.

The context initializes on its presentation thread before open succeeds; missing
snapshot support reports `gpu_snapshot_unavailable`. A failed copy during a
stream faults it as `gpu_snapshot_failed`. Fresh allocation and synchronous GPU
completion favor ownership correctness; sustained cost and multi-GPU import need
hardware measurement. Ten-bit storage is not an HDR qualification: the current
KMS frame metadata remains sRGB, and HDR colorimetry, encoder support and client
playback remain separate unfinished gates.

### Threading, latency and lifetimes

- Reads and queries take the controller lock with `try_lock` (busy rather
  than blocking the host tick) and the stream's slot lock only to copy a
  reference to the stored frame. No lock is held while the presentation thread
  waits on the helper, and the presentation thread never waits for a sink:
  a read holds only that stream's read gate while the sink runs.
- Without snapshots, a presentation is readable one FRAME round trip (a
  helper exchange and the driver export, bounded to 100 ms) after WAIT returns, which is itself
  immediate on completion. The consumer then sees it on its next
  `latest_presentation` poll.
- Snapshot mode additionally waits for input fences and GPU copy completion;
  a stalled graphics driver can delay teardown. No bounded GPU execution time
  is claimed from the helper's socket timeout.
- The thread stops on close, on module shutdown, and on its own when the
  stream faults. Close sets a stop flag, shuts down the stream's helper socket,
  which makes a pending WAIT or FRAME fail at once, and joins the thread; the
  thread then ends the helper (it exits on end of stream, or is killed after
  50 ms if it is still in the driver). Close therefore blocks the tick for
  about 50 ms for the legacy helper outside a GPU-copy operation. Approved
  sessions are terminated by their broker when the module closes its endpoint.
- `busy` and `interrupted` driver refusals are retried after 5 ms; other
  driver refusals after 20 ms, faulting the stream as `frame_helper_failed`
  after 10 in a row. `crtc_not_found` asks the controller to re-resolve the
  output on its next tick and retries after 100 ms or as soon as the
  controller retargets the stream. A closed, silent (the WAIT bound plus
  250 ms) or out-of-step helper faults the stream as `frame_helper_failed`,
  and `permission_denied` as `permission_denied`.

## Helper protocol

The module creates an AF_UNIX `SOCK_SEQPACKET` socketpair and starts the helper
with an empty environment, no arguments, null standard streams, `/` as working
directory and its socket end at descriptor 3; every other descriptor is
close-on-exec (Linux 5.11 or later). The helper exits when the socket closes,
and exit statuses 64-69 identify startup refusals.

Every record is one datagram of fixed length, little-endian, version 1. All
fields an operation does not use and every reserved byte must be zero;
anything else, a wrong length, or a request carrying descriptors receives a
`malformed_request` failure without touching the device.

Requests are 48 bytes: magic `AVFQ`, version u16, opcode u16, request id u32,
crtc id u32, device major u32, device minor u32, last sequence u64, timeout
u32, 12 reserved bytes. Opcodes:

1. HELLO: answers whether `CAP_SYS_ADMIN` is permitted.
2. OPEN_CARD(major, minor): opens `/dev/dri/card<minor>` read-write, requires
   major 226, the same `st_rdev` and the driver name `vibeshine_drm`, and drops
   any master role. One card per helper.
3. FRAME(crtc): driver GET_FRAME.
4. WAIT(crtc, last sequence, timeout <= 1000 ms): driver WAIT_PRESENT. It
   returns as soon as a newer sequence completes, or flags a timeout.

Responses are 128 bytes: magic `AVFR`, version, echoed opcode and request id,
status u32 (0 or a failure), errno detail, result flags, crtc id, plane count,
sequence, timestamp, width, height, fourcc, fence mask, modifier, four pitches,
four offsets and 24 reserved bytes. A ready frame carries one SCM_RIGHTS
message: each plane's DMA-BUF in plane order, then the sync_file of each plane
named in the fence mask. The helper closes its own copies after sending. The
client checks the request id, opcode, CRTC echo and exact descriptor count, and
stops using a helper after any transport or protocol fault. Another thread can
end a pending request through a shutdown handle that holds its own duplicate of
the socket; the request then fails as closed and the helper sees end of stream.

Failures are distinct: `permission_denied` (no permitted `CAP_SYS_ADMIN` or the
driver refused it), `card_not_open`, `card_already_open`, `device_rejected`,
`device_unavailable`, `wrong_driver`, `crtc_not_found`, `busy`, `interrupted`,
`driver_error` and `invalid_driver_response`. The helper validates the driver's
answer as strictly as a request and closes every descriptor of a malformed one.

The helper sets `PR_SET_NO_NEW_PRIVS`, clears dumpability and the ambient set,
reduces its capabilities to at most a permitted `CAP_SYS_ADMIN` with nothing
effective or inheritable, and raises it to effective only for the duration of
one private ioctl, lowering it immediately (it aborts if lowering fails). It
never takes DRM master, commits state or writes properties.

## Qualification

Unit tests cover the wire protocol, descriptor passing over a socketpair,
the helper's request rules and capability handling with fakes, output names,
sysfs scanning of a fake tree, resolution and error mapping, desktop geometry
from fake compositor layouts (scaled primary, appended-right, negative origins,
mode mismatch, missing outputs, unsupported and not-ready states), freshness and
timestamps, descriptor lifetimes across the native callback and across frame
replacement, the presentation thread against a fake helper (push on each
presentation, coalescing, empty CRTCs, monotonic presentation queries, fault
propagation, re-resolution after `crtc_not_found`, and stop latency with and
without interruptible helpers), the socket shutdown handle, and helper-file
checks. An integration test runs the real helper binary without capabilities
and never asks it to open a DRM device. No test connects to the compositor; the
live layout test is ignored. Capture from a real vibeshine_drm
device with `CAP_SYS_ADMIN`, the exported formats and modifiers KWin produces,
fence behavior, presentation-to-availability latency and VRR cadence on real
hardware, and the video and encoder import path remain separate acceptance
gates. Behavior was modeled on Vibeshine's KMS capture; no Vibeshine
source was copied.
