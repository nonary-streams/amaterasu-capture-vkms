#define _POSIX_C_SOURCE 200809L
#include "wayland_outputs.h"
#include "xdg-output-unstable-v1.h"
#include <wayland-client.h>
#include <errno.h>
#include <poll.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define MAX_OUTPUTS 64

struct am_wl;

struct output {
    struct am_wl *owner;
    uint32_t global;
    struct wl_output *object;
    struct zxdg_output_v1 *xdg;
    struct am_wl_output state;
    bool live;
};

struct am_wl {
    struct wl_display *display;
    struct wl_registry *registry;
    struct zxdg_output_manager_v1 *xdg_manager;
    struct output outputs[MAX_OUTPUTS];
    size_t count;
    uint64_t generation;
    bool failed;
};

static void set_name(struct output *output, const char *name) {
    if (name && strlen(name) < sizeof(output->state.name)) memcpy(output->state.name, name, strlen(name) + 1);
}

static void output_geometry(void *data, struct wl_output *object, int32_t x, int32_t y, int32_t physical_width,
                            int32_t physical_height, int32_t subpixel, const char *make, const char *model,
                            int32_t transform) {
    (void)data; (void)object; (void)x; (void)y; (void)physical_width; (void)physical_height;
    (void)subpixel; (void)make; (void)model; (void)transform;
}
static void output_mode(void *data, struct wl_output *object, uint32_t flags, int32_t width, int32_t height, int32_t refresh) {
    (void)object; (void)refresh;
    struct output *output = data;
    if (flags & WL_OUTPUT_MODE_CURRENT) {
        output->state.mode_width = width;
        output->state.mode_height = height;
        output->state.has_mode = 1;
    }
}
/* wl_output.done ends every atomic group of output property changes. */
static void output_done(void *data, struct wl_output *object);
static void output_scale(void *data, struct wl_output *object, int32_t factor) { (void)data; (void)object; (void)factor; }
static void output_name(void *data, struct wl_output *object, const char *name) { (void)object; set_name(data, name); }
static void output_description(void *data, struct wl_output *object, const char *description) {
    (void)data; (void)object; (void)description;
}
static const struct wl_output_listener output_listener = {
    .geometry = output_geometry, .mode = output_mode, .done = output_done,
    .scale = output_scale, .name = output_name, .description = output_description,
};

static void logical_position(void *data, struct zxdg_output_v1 *object, int32_t x, int32_t y) {
    (void)object;
    struct output *output = data;
    output->state.logical_left = x;
    output->state.logical_top = y;
}
static void logical_size(void *data, struct zxdg_output_v1 *object, int32_t width, int32_t height) {
    (void)object;
    struct output *output = data;
    output->state.logical_width = width;
    output->state.logical_height = height;
}
static void logical_done(void *data, struct zxdg_output_v1 *object) { output_done(data, NULL); (void)object; }
static void logical_name(void *data, struct zxdg_output_v1 *object, const char *name) { (void)object; set_name(data, name); }
static void logical_description(void *data, struct zxdg_output_v1 *object, const char *description) {
    (void)data; (void)object; (void)description;
}
static const struct zxdg_output_v1_listener logical_listener = {
    .logical_position = logical_position, .logical_size = logical_size, .done = logical_done,
    .name = logical_name, .description = logical_description,
};

static void output_done(void *data, struct wl_output *object) {
    (void)object;
    struct output *output = data;
    if (output->owner) output->owner->generation++;
}

static void attach_xdg(struct am_wl *connection, struct output *output) {
    if (!connection->xdg_manager || output->xdg || !output->object) return;
    output->xdg = zxdg_output_manager_v1_get_xdg_output(connection->xdg_manager, output->object);
    zxdg_output_v1_add_listener(output->xdg, &logical_listener, output);
}

static void release_output(struct output *output) {
    if (output->xdg) zxdg_output_v1_destroy(output->xdg);
    if (output->object) {
        if (wl_output_get_version(output->object) >= 3) wl_output_release(output->object);
        else wl_output_destroy(output->object);
    }
    memset(output, 0, sizeof(*output));
}

static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    struct am_wl *connection = data;
    if (strcmp(interface, zxdg_output_manager_v1_interface.name) == 0 && !connection->xdg_manager) {
        connection->xdg_manager = wl_registry_bind(registry, name, &zxdg_output_manager_v1_interface, version < 3 ? version : 3);
        for (size_t i = 0; i < connection->count; i++) attach_xdg(connection, &connection->outputs[i]);
    } else if (strcmp(interface, wl_output_interface.name) == 0) {
        if (version < 2) return;
        size_t index = 0;
        while (index < connection->count && connection->outputs[index].live) ++index;
        if (index >= MAX_OUTPUTS) { connection->failed = true; return; }
        if (index == connection->count) ++connection->count;
        struct output *output = &connection->outputs[index];
        release_output(output);
        output->owner = connection;
        output->global = name;
        output->live = true;
        output->object = wl_registry_bind(registry, name, &wl_output_interface, version < 4 ? version : 4);
        wl_output_add_listener(output->object, &output_listener, output);
        attach_xdg(connection, output);
        connection->generation++;
    }
}

static void global_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)registry;
    struct am_wl *connection = data;
    for (size_t i = 0; i < connection->count; i++) {
        if (connection->outputs[i].live && connection->outputs[i].global == name) {
            release_output(&connection->outputs[i]);
            connection->generation++;
        }
    }
}
static const struct wl_registry_listener registry_listener = {.global = global, .global_remove = global_remove};

static int dispatch(struct am_wl *connection, int timeout_ms) {
    if (!connection || connection->failed) return -1;
    while (wl_display_prepare_read(connection->display) != 0) {
        if (wl_display_dispatch_pending(connection->display) < 0) return -1;
    }
    if (wl_display_flush(connection->display) < 0 && errno != EAGAIN) {
        wl_display_cancel_read(connection->display);
        return -1;
    }
    struct pollfd socket = {.fd = wl_display_get_fd(connection->display), .events = POLLIN};
    int ready = poll(&socket, 1, timeout_ms);
    if (ready > 0 && (socket.revents & POLLIN)) {
        if (wl_display_read_events(connection->display) < 0) return -1;
    } else {
        wl_display_cancel_read(connection->display);
        if ((ready < 0 && errno != EINTR) || (ready > 0 && (socket.revents & (POLLERR | POLLHUP | POLLNVAL)))) return -1;
    }
    return wl_display_dispatch_pending(connection->display) < 0 || connection->failed ? -1 : 0;
}

static int64_t now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

static void synchronized(void *data, struct wl_callback *callback, uint32_t serial) {
    (void)callback; (void)serial;
    *(bool *)data = true;
}
static const struct wl_callback_listener sync_listener = {.done = synchronized};

static int roundtrip(struct am_wl *connection, int64_t deadline) {
    bool done = false;
    struct wl_callback *callback = wl_display_sync(connection->display);
    if (!callback) return -1;
    wl_callback_add_listener(callback, &sync_listener, &done);
    while (!done) {
        int64_t remaining = deadline - now_ms();
        if (remaining <= 0 || dispatch(connection, (int)(remaining < 10 ? remaining : 10)) < 0) break;
    }
    wl_callback_destroy(callback);
    return done ? 0 : -1;
}

struct am_wl *am_wl_open(int timeout_ms) {
    struct am_wl *connection = calloc(1, sizeof(*connection));
    if (!connection) return NULL;
    int64_t deadline = now_ms() + (timeout_ms > 0 ? timeout_ms : 0);
    connection->display = wl_display_connect(NULL);
    if (!connection->display) goto failure;
    connection->registry = wl_display_get_registry(connection->display);
    wl_registry_add_listener(connection->registry, &registry_listener, connection);
    /* The first roundtrip announces globals; the second delivers output events. */
    if (roundtrip(connection, deadline) < 0 || !connection->xdg_manager) goto failure;
    if (roundtrip(connection, deadline) < 0) goto failure;
    return connection;
failure:
    am_wl_close(connection);
    return NULL;
}

int am_wl_poll(struct am_wl *connection) {
    return dispatch(connection, 0);
}

uint64_t am_wl_generation(const struct am_wl *connection) { return connection ? connection->generation : 0; }

size_t am_wl_output_slots(const struct am_wl *connection) { return connection ? connection->count : 0; }

int am_wl_output_at(const struct am_wl *connection, size_t slot, struct am_wl_output *output) {
    if (!connection || !output || slot >= connection->count || !connection->outputs[slot].live) return -1;
    *output = connection->outputs[slot].state;
    output->name[sizeof(output->name) - 1] = '\0';
    return 0;
}

void am_wl_close(struct am_wl *connection) {
    if (!connection) return;
    for (size_t i = 0; i < connection->count; i++) release_output(&connection->outputs[i]);
    if (connection->xdg_manager) zxdg_output_manager_v1_destroy(connection->xdg_manager);
    if (connection->registry) wl_registry_destroy(connection->registry);
    if (connection->display) {
        wl_display_flush(connection->display);
        wl_display_disconnect(connection->display);
    }
    free(connection);
}
