#ifndef AMATERASU_VKMS_GPU_SNAPSHOT_H
#define AMATERASU_VKMS_GPU_SNAPSHOT_H

#include <stdint.h>

struct am_vkms_snapshotter;

struct am_vkms_snapshot_input {
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;
    uint32_t plane_count;
    uint64_t modifier;
    int32_t fds[4];
    uint32_t pitches[4];
    uint32_t offsets[4];
    int32_t acquire_fences[4];
};

struct am_vkms_snapshot_output {
    int32_t fd;
    uint32_t pitch;
    uint32_t fourcc;
    int32_t completion_fd;
    uint64_t modifier;
};

_Static_assert(sizeof(struct am_vkms_snapshot_input) == 88, "snapshot input ABI");
_Static_assert(sizeof(struct am_vkms_snapshot_output) == 24, "snapshot output ABI");

/* All calls and destruction must happen on the thread that created the object.
 * Success returns an owned DMA-BUF and, when supported, an owned sync_file.
 * The allocation is written once; consumers must honor its completion fence.
 * The caller retains the input descriptors and guards until poll returns 1.
 * poll returns 0 while pending, -2 on failure, and never releases an unproven
 * source. A zero timeout is nonblocking; only teardown supplies a timeout.
 * RGB8 sources retain 8-bit storage, and RGB10 sources retain 10-bit
 * storage in an equivalent supported RGB layout. Unsupported import/render formats fail, with no downgrade. */
struct am_vkms_snapshotter *am_vkms_snapshot_create(void);
int am_vkms_snapshot_copy(struct am_vkms_snapshotter *,
                          const struct am_vkms_snapshot_input *,
                          struct am_vkms_snapshot_output *);
int am_vkms_snapshot_poll(struct am_vkms_snapshotter *, uint64_t timeout_ns);
void am_vkms_snapshot_destroy(struct am_vkms_snapshotter *);

#endif
