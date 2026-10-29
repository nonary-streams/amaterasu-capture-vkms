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
};

struct am_vkms_snapshot_output {
    int32_t fd;
    uint32_t pitch;
    uint32_t fourcc;
    uint32_t reserved;
    uint64_t modifier;
};

_Static_assert(sizeof(struct am_vkms_snapshot_input) == 72, "snapshot input ABI");
_Static_assert(sizeof(struct am_vkms_snapshot_output) == 24, "snapshot output ABI");

/* All calls and destruction must happen on the thread that created the object.
 * Inputs remain borrowed through copy. Success returns one owned DMA-BUF fd:
 * the GPU copy is complete and this allocation is never written again.
 * RGB8 sources retain 8-bit storage (XR24), and RGB10 sources retain 10-bit
 * storage (XR30). Unsupported import/render formats fail, with no downgrade. */
struct am_vkms_snapshotter *am_vkms_snapshot_create(void);
int am_vkms_snapshot_copy(struct am_vkms_snapshotter *,
                          const struct am_vkms_snapshot_input *,
                          struct am_vkms_snapshot_output *);
void am_vkms_snapshot_destroy(struct am_vkms_snapshotter *);

#endif
