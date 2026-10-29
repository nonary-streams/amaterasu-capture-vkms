#ifndef AMATERASU_VKMS_WAYLAND_OUTPUTS_H
#define AMATERASU_VKMS_WAYLAND_OUTPUTS_H
#include <stddef.h>
#include <stdint.h>

/* Read-only view of the compositor's outputs: wl_registry, wl_output and
 * xdg-output only. No other protocol is bound. A connection and its proxies
 * are used from one thread at a time; the caller serializes access. */
struct am_wl;

struct am_wl_output {
    char name[128];
    int32_t logical_left, logical_top, logical_width, logical_height;
    int32_t mode_width, mode_height;
    uint32_t has_mode;
};

/* Connects to $WAYLAND_DISPLAY and completes the initial roundtrips within
 * timeout_ms. NULL when no display, no xdg-output manager, or no answer. */
struct am_wl *am_wl_open(int timeout_ms);
/* Dispatches pending events without blocking; -1 when the connection failed. */
int am_wl_poll(struct am_wl *connection);
/* Increments on every output done, removal or addition. */
uint64_t am_wl_generation(const struct am_wl *connection);
size_t am_wl_output_slots(const struct am_wl *connection);
/* Copies a live output; returns 0, or -1 for a removed or empty slot. */
int am_wl_output_at(const struct am_wl *connection, size_t slot, struct am_wl_output *output);
void am_wl_close(struct am_wl *connection);
#endif
