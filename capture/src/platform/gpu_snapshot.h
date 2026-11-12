#ifndef AMATERASU_VKMS_GPU_SNAPSHOT_H
#define AMATERASU_VKMS_GPU_SNAPSHOT_H

#include <stdint.h>

struct am_vkms_snapshotter;

/* Number of private destination buffers per snapshotter (one stream). */
#define AM_VKMS_SNAPSHOT_SLOTS 4u

/* Copy outcomes. */
#define AM_VKMS_SNAPSHOT_OK 0
#define AM_VKMS_SNAPSHOT_UNSUPPORTED (-1)
#define AM_VKMS_SNAPSHOT_BACKEND_ERROR (-2)
/* A source acquire fence could not be queued as a GPU wait. Nothing was
 * drawn; the stream fails without a CPU wait. */
#define AM_VKMS_SNAPSHOT_ACQUIRE_NOT_QUEUED (-3)

/* Output flags describing the work this copy needed. */
#define AM_VKMS_SNAPSHOT_SOURCE_CACHED 1u
#define AM_VKMS_SNAPSHOT_SLOT_ALLOCATED 2u

struct am_vkms_snapshot_input {
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;
    uint32_t plane_count;
    uint64_t modifier;
    int32_t fds[4];
    uint32_t pitches[4];
    uint32_t offsets[4];
    /* Borrowed sync_file fences for the source planes, or -1. */
    int32_t acquire_fence_fds[4];
    /* Destination slot, below AM_VKMS_SNAPSHOT_SLOTS. */
    uint32_t slot;
    uint32_t reserved;
};

struct am_vkms_snapshot_output {
    /* New descriptor for the slot's private DMA-BUF, owned by the caller. */
    int32_t fd;
    /* sync_file that signals when the copy is complete, owned by the caller,
     * or -1 on failure. A successful copy always supplies a fence. */
    int32_t fence_fd;
    uint32_t pitch;
    uint32_t fourcc;
    uint64_t modifier;
    uint32_t flags;
    uint32_t reserved;
};

_Static_assert(sizeof(struct am_vkms_snapshot_input) == 96, "snapshot input ABI");
_Static_assert(sizeof(struct am_vkms_snapshot_output) == 32, "snapshot output ABI");

/* All calls and destruction must happen on the thread that created the object.
 *
 * The caller owns slot scheduling: it may name a slot only after every
 * consumer of that slot's previous contents has released it (including any
 * consumer completion fence). Copies into a slot are ordered after earlier
 * copies on the same private context. The snapshotter allocates, imports and
 * exports each slot once per geometry/format, and caches source imports by
 * DMA-BUF identity (device, inode and layout) while holding a duplicate
 * descriptor, so a cached identity cannot be recycled.
 *
 * Inputs remain borrowed through copy. On success the copy has been submitted,
 * not necessarily completed: `fence_fd` signals completion. RGB8 sources retain
 * 8-bit storage, and RGB10 sources retain 10-bit storage in an equivalent
 * supported RGB layout. Unsupported import/render formats fail, with no
 * downgrade. Inputs and destination claims must remain retained until poll
 * proves completion. Destroy uses a bounded wait and retains an unproven copy;
 * exported descriptors keep their storage alive independently. */
struct am_vkms_snapshotter *am_vkms_snapshot_create(void);
int am_vkms_snapshot_copy(struct am_vkms_snapshotter *,
                          const struct am_vkms_snapshot_input *,
                          struct am_vkms_snapshot_output *);
void am_vkms_snapshot_destroy(struct am_vkms_snapshotter *);

/* Owner-thread nonblocking completion observation; 1 retired, 0 pending, negative error. */
int am_vkms_snapshot_poll(struct am_vkms_snapshotter *snapshotter, uint64_t timeout_ns);

#endif
