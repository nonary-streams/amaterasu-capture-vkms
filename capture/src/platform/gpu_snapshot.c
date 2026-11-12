#define _GNU_SOURCE
#include "gpu_snapshot.h"
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <drm_fourcc.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define SOURCE_CACHE_CAPACITY 8u

/* One private destination buffer. The exported descriptor `fd` is the
 * snapshotter's own reference; every copy hands the caller a duplicate. */
struct snapshot_slot {
    struct gbm_bo *buffer;
    int fd;
    EGLImageKHR image;
    GLuint texture;
    GLuint framebuffer;
    uint32_t width;
    uint32_t height;
    /* GBM_FORMAT_XRGB8888 or GBM_FORMAT_XRGB2101010: the precision class. */
    uint32_t precision_format;
    uint32_t fourcc;
    uint32_t pitch;
    uint64_t modifier;
};

/* One imported source DMA-BUF. The duplicated descriptors keep the identity
 * (device, inode) from being recycled while the entry is cached. */
struct cached_source {
    bool used;
    uint64_t last_use;
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;
    uint32_t plane_count;
    uint64_t modifier;
    uint32_t pitches[4];
    uint32_t offsets[4];
    dev_t devices[4];
    ino_t inodes[4];
    int fds[4];
    EGLImageKHR image;
    GLuint texture;
};

struct am_vkms_snapshotter {
    int render_fd;
    struct gbm_device *gbm;
    EGLDisplay display;
    EGLContext context;
    EGLSurface surface;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
    PFNEGLCREATESYNCKHRPROC create_sync;
    PFNEGLDESTROYSYNCKHRPROC destroy_sync;
    PFNEGLWAITSYNCKHRPROC wait_sync;
    PFNEGLCLIENTWAITSYNCKHRPROC client_wait_sync;
    bool pending_active;
    EGLSyncKHR pending_completion;
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_native_fence;
    GLuint program;
    GLint position;
    GLint coordinates;
    GLint sampler;
    bool modifiers;
    /* Copies return a sync_file completion fence instead of glFinish. */
    bool completion_fences;
    /* Source acquire fences become GPU-side waits. */
    bool acquire_waits;
    uint64_t use_clock;
    struct snapshot_slot slots[AM_VKMS_SNAPSHOT_SLOTS];
    struct cached_source sources[SOURCE_CACHE_CAPACITY];
};

static bool extension(const char *extensions, const char *name) {
    if (extensions == NULL || name == NULL || *name == '\0') return false;
    size_t length = strlen(name);
    const char *at = extensions;
    while ((at = strstr(at, name)) != NULL) {
        if ((at == extensions || at[-1] == ' ') &&
            (at[length] == '\0' || at[length] == ' ')) return true;
        at += length;
    }
    return false;
}

static GLuint shader(GLenum type, const char *source) {
    GLuint value = glCreateShader(type);
    if (value == 0) return 0;
    glShaderSource(value, 1, &source, NULL);
    glCompileShader(value);
    GLint compiled = GL_FALSE;
    glGetShaderiv(value, GL_COMPILE_STATUS, &compiled);
    if (compiled == GL_TRUE) return value;
    glDeleteShader(value);
    return 0;
}

static bool prepare_program(struct am_vkms_snapshotter *snapshotter) {
    static const char vertex[] =
        "attribute vec2 position; attribute vec2 coordinates;"
        "varying vec2 source_coordinates;"
        "void main() { gl_Position=vec4(position,0.0,1.0);"
        "source_coordinates=coordinates; }";
    static const char fragment[] =
        "#extension GL_OES_EGL_image_external : require\n"
        "precision highp float; varying vec2 source_coordinates;"
        "uniform highp samplerExternalOES source_texture;"
        "void main() { gl_FragColor=texture2D(source_texture,source_coordinates); }";
    GLuint vs = shader(GL_VERTEX_SHADER, vertex);
    GLuint fs = shader(GL_FRAGMENT_SHADER, fragment);
    if (vs == 0 || fs == 0) {
        if (vs != 0) glDeleteShader(vs);
        if (fs != 0) glDeleteShader(fs);
        return false;
    }
    snapshotter->program = glCreateProgram();
    glAttachShader(snapshotter->program, vs);
    glAttachShader(snapshotter->program, fs);
    glLinkProgram(snapshotter->program);
    glDeleteShader(vs);
    glDeleteShader(fs);
    GLint linked = GL_FALSE;
    glGetProgramiv(snapshotter->program, GL_LINK_STATUS, &linked);
    if (linked != GL_TRUE) return false;
    snapshotter->position = glGetAttribLocation(snapshotter->program, "position");
    snapshotter->coordinates = glGetAttribLocation(snapshotter->program, "coordinates");
    snapshotter->sampler = glGetUniformLocation(snapshotter->program, "source_texture");
    return snapshotter->position >= 0 && snapshotter->coordinates >= 0 && snapshotter->sampler >= 0;
}

static bool initialize(struct am_vkms_snapshotter *snapshotter) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (get_display == NULL) return false;
    snapshotter->display = get_display(EGL_PLATFORM_GBM_KHR, snapshotter->gbm, NULL);
    if (snapshotter->display == EGL_NO_DISPLAY ||
        eglInitialize(snapshotter->display, NULL, NULL) != EGL_TRUE ||
        eglBindAPI(EGL_OPENGL_ES_API) != EGL_TRUE) return false;
    const char *extensions = eglQueryString(snapshotter->display, EGL_EXTENSIONS);
    if (!extension(extensions, "EGL_EXT_image_dma_buf_import")) return false;
    snapshotter->modifiers = extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers");
    snapshotter->create_image = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    snapshotter->destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    snapshotter->bind_image = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (snapshotter->create_image == NULL || snapshotter->destroy_image == NULL ||
        snapshotter->bind_image == NULL) return false;
    /* Explicit synchronization is optional: without it every copy finishes on
     * the CPU before returning, and acquire fences are waited by the caller. */
    if (extension(extensions, "EGL_KHR_fence_sync") &&
        extension(extensions, "EGL_ANDROID_native_fence_sync")) {
        snapshotter->create_sync = (PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
        snapshotter->destroy_sync = (PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
        snapshotter->dup_native_fence =
            (PFNEGLDUPNATIVEFENCEFDANDROIDPROC)eglGetProcAddress("eglDupNativeFenceFDANDROID");
        snapshotter->completion_fences = snapshotter->create_sync != NULL &&
                                         snapshotter->destroy_sync != NULL &&
                                         snapshotter->dup_native_fence != NULL;
        if (snapshotter->completion_fences && extension(extensions, "EGL_KHR_wait_sync")) {
            snapshotter->wait_sync = (PFNEGLWAITSYNCKHRPROC)eglGetProcAddress("eglWaitSyncKHR");
            snapshotter->acquire_waits = snapshotter->wait_sync != NULL;
        }
    }
    snapshotter->client_wait_sync = (PFNEGLCLIENTWAITSYNCKHRPROC)eglGetProcAddress("eglClientWaitSyncKHR");
    if (!snapshotter->completion_fences || !snapshotter->acquire_waits ||
        !snapshotter->client_wait_sync) return false;
    const EGLint attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE
    };
    EGLConfig config;
    EGLint count = 0;
    if (eglChooseConfig(snapshotter->display, attributes, &config, 1, &count) != EGL_TRUE || count != 1)
        return false;
    const EGLint surface[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    const EGLint context[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    const EGLint prioritized[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_CONTEXT_PRIORITY_LEVEL_IMG,
                                  EGL_CONTEXT_PRIORITY_HIGH_IMG, EGL_NONE};
    const bool priority = extension(extensions, "EGL_IMG_context_priority");
    snapshotter->surface = eglCreatePbufferSurface(snapshotter->display, config, surface);
    snapshotter->context = EGL_NO_CONTEXT;
    if (priority)
        snapshotter->context =
            eglCreateContext(snapshotter->display, config, EGL_NO_CONTEXT, prioritized);
    if (snapshotter->context == EGL_NO_CONTEXT)
        snapshotter->context = eglCreateContext(snapshotter->display, config, EGL_NO_CONTEXT, context);
    EGLint granted = 0;
    if (priority && snapshotter->context != EGL_NO_CONTEXT)
        (void)eglQueryContext(snapshotter->display, snapshotter->context,
                              EGL_CONTEXT_PRIORITY_LEVEL_IMG, &granted);
    fprintf(stderr, "amaterasu vkms snapshot: GPU priority=%s requested=high\n",
            granted == EGL_CONTEXT_PRIORITY_HIGH_IMG     ? "high"
            : granted == EGL_CONTEXT_PRIORITY_MEDIUM_IMG ? "medium"
            : granted == EGL_CONTEXT_PRIORITY_LOW_IMG    ? "low"
                                                         : "unavailable");
    if (snapshotter->surface == EGL_NO_SURFACE || snapshotter->context == EGL_NO_CONTEXT ||
        eglMakeCurrent(snapshotter->display, snapshotter->surface, snapshotter->surface,
                       snapshotter->context) != EGL_TRUE) return false;
    const char *gl_extensions = (const char *)glGetString(GL_EXTENSIONS);
    return extension(gl_extensions, "GL_OES_EGL_image") &&
           extension(gl_extensions, "GL_OES_EGL_image_external") && prepare_program(snapshotter);
}

struct am_vkms_snapshotter *am_vkms_snapshot_create(void) {
    /* Render nodes never confer DRM master or display-management authority. */
    for (unsigned minor = 128; minor < 192; ++minor) {
        char path[64];
        (void)snprintf(path, sizeof(path), "/dev/dri/renderD%u", minor);
        int fd = open(path, O_RDWR | O_CLOEXEC | O_NOFOLLOW);
        if (fd < 0) continue;
        struct stat facts;
        if (fstat(fd, &facts) != 0 || !S_ISCHR(facts.st_mode)) { close(fd); continue; }
        struct am_vkms_snapshotter *snapshotter = calloc(1, sizeof(*snapshotter));
        if (snapshotter == NULL) { close(fd); return NULL; }
        snapshotter->render_fd = fd;
        for (unsigned slot = 0; slot < AM_VKMS_SNAPSHOT_SLOTS; ++slot)
            snapshotter->slots[slot].fd = -1;
        for (unsigned entry = 0; entry < SOURCE_CACHE_CAPACITY; ++entry)
            for (unsigned plane = 0; plane < 4; ++plane)
                snapshotter->sources[entry].fds[plane] = -1;
        snapshotter->display = EGL_NO_DISPLAY;
        snapshotter->context = EGL_NO_CONTEXT;
        snapshotter->surface = EGL_NO_SURFACE;
        snapshotter->gbm = gbm_create_device(fd);
        if (snapshotter->gbm != NULL && initialize(snapshotter)) return snapshotter;
        am_vkms_snapshot_destroy(snapshotter);
    }
    return NULL;
}

static EGLImageKHR import_image(struct am_vkms_snapshotter *snapshotter,
                                const struct am_vkms_snapshot_input *input) {
    static const EGLint fd_fields[] = {EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE1_FD_EXT,
                                      EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE3_FD_EXT};
    static const EGLint offset_fields[] = {EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT,
                                          EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT};
    static const EGLint pitch_fields[] = {EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT,
                                         EGL_DMA_BUF_PLANE2_PITCH_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT};
    static const EGLint low_fields[] = {EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT,
                                       EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT};
    static const EGLint high_fields[] = {EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT,
                                        EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT};
    if (input->modifier != DRM_FORMAT_MOD_INVALID && !snapshotter->modifiers &&
        input->modifier != DRM_FORMAT_MOD_LINEAR) return EGL_NO_IMAGE_KHR;
    EGLint attributes[64];
    size_t index = 0;
    attributes[index++] = EGL_WIDTH; attributes[index++] = (EGLint)input->width;
    attributes[index++] = EGL_HEIGHT; attributes[index++] = (EGLint)input->height;
    attributes[index++] = EGL_LINUX_DRM_FOURCC_EXT; attributes[index++] = (EGLint)input->fourcc;
    for (uint32_t plane = 0; plane < input->plane_count; ++plane) {
        attributes[index++] = fd_fields[plane]; attributes[index++] = input->fds[plane];
        attributes[index++] = offset_fields[plane]; attributes[index++] = (EGLint)input->offsets[plane];
        attributes[index++] = pitch_fields[plane]; attributes[index++] = (EGLint)input->pitches[plane];
        if (snapshotter->modifiers && input->modifier != DRM_FORMAT_MOD_INVALID) {
            attributes[index++] = low_fields[plane]; attributes[index++] = (EGLint)(uint32_t)input->modifier;
            attributes[index++] = high_fields[plane]; attributes[index++] = (EGLint)(uint32_t)(input->modifier >> 32);
        }
    }
    attributes[index] = EGL_NONE;
    return snapshotter->create_image(snapshotter->display, EGL_NO_CONTEXT,
                                    EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
}

/* Some drivers expose AR30/XB30 but reject XR30. Try equivalent RGB layouts
 * at the requested precision; never fall back from ten bits to eight bits. */
static struct gbm_bo *allocate_snapshot(struct gbm_device *device,
                                        uint32_t width, uint32_t height,
                                        uint32_t *format) {
    static const uint32_t rgb8[] = {GBM_FORMAT_XRGB8888, GBM_FORMAT_ARGB8888,
                                    GBM_FORMAT_XBGR8888, GBM_FORMAT_ABGR8888};
    static const uint32_t rgb10[] = {GBM_FORMAT_XRGB2101010, GBM_FORMAT_ARGB2101010,
                                     GBM_FORMAT_XBGR2101010, GBM_FORMAT_ABGR2101010};
    const uint32_t *formats = *format == GBM_FORMAT_XRGB2101010 ? rgb10 : rgb8;
    for (unsigned i = 0; i < 4; ++i) {
        struct gbm_bo *buffer = gbm_bo_create(device, width, height, formats[i],
                                             GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
        if (buffer == NULL)
            buffer = gbm_bo_create(device, width, height, formats[i], GBM_BO_USE_RENDERING);
        if (buffer != NULL && gbm_bo_get_plane_count(buffer) == 1) {
            *format = formats[i];
            return buffer;
        }
        if (buffer != NULL) gbm_bo_destroy(buffer);
    }
    return NULL;
}

static uint32_t precision_format(uint32_t fourcc) {
    /* Keep the source's RGB precision. Ten-bit storage does not imply HDR:
     * transfer function and primaries remain the caller's separate contract.
     * YUV sources are refused rather than guessing matrix/range metadata. */
    switch (fourcc) {
    case DRM_FORMAT_XRGB8888:
    case DRM_FORMAT_ARGB8888:
    case DRM_FORMAT_XBGR8888:
    case DRM_FORMAT_ABGR8888:
        return GBM_FORMAT_XRGB8888;
    case DRM_FORMAT_XRGB2101010:
    case DRM_FORMAT_ARGB2101010:
    case DRM_FORMAT_XBGR2101010:
    case DRM_FORMAT_ABGR2101010:
        return GBM_FORMAT_XRGB2101010;
    default:
        return 0;
    }
}

/* Requires the private context to be current. */
static void release_slot(struct am_vkms_snapshotter *snapshotter, struct snapshot_slot *slot) {
    if (slot->framebuffer != 0) glDeleteFramebuffers(1, &slot->framebuffer);
    if (slot->texture != 0) glDeleteTextures(1, &slot->texture);
    if (slot->image != EGL_NO_IMAGE_KHR) snapshotter->destroy_image(snapshotter->display, slot->image);
    if (slot->fd >= 0) close(slot->fd);
    if (slot->buffer != NULL) gbm_bo_destroy(slot->buffer);
    memset(slot, 0, sizeof(*slot));
    slot->fd = -1;
}

/* Allocates, exports and imports a slot once per geometry and precision.
 * A slot already matching the request is reused without any allocation. */
static int prepare_slot(struct am_vkms_snapshotter *snapshotter, struct snapshot_slot *slot,
                        uint32_t width, uint32_t height, uint32_t precision, bool *allocated) {
    *allocated = false;
    if (slot->buffer != NULL && slot->width == width && slot->height == height &&
        slot->precision_format == precision) return AM_VKMS_SNAPSHOT_OK;
    release_slot(snapshotter, slot);
    uint32_t format = precision;
    slot->buffer = allocate_snapshot(snapshotter->gbm, width, height, &format);
    if (slot->buffer == NULL || gbm_bo_get_plane_count(slot->buffer) != 1) goto unsupported;
    slot->fd = gbm_bo_get_fd(slot->buffer);
    if (slot->fd < 0 || fcntl(slot->fd, F_SETFD, FD_CLOEXEC) != 0) goto unsupported;
    /* This implementation only exports a single-plane buffer beginning at zero. */
    if (gbm_bo_get_offset(slot->buffer, 0) != 0) goto unsupported;
    slot->width = width;
    slot->height = height;
    slot->precision_format = precision;
    slot->fourcc = format;
    slot->pitch = gbm_bo_get_stride(slot->buffer);
    slot->modifier = gbm_bo_get_modifier(slot->buffer);
    struct am_vkms_snapshot_input destination = {
        .width = width, .height = height, .fourcc = format,
        .plane_count = 1, .modifier = slot->modifier,
        .fds = {slot->fd, -1, -1, -1},
        .pitches = {slot->pitch, 0, 0, 0},
        .offsets = {0, 0, 0, 0},
        .acquire_fence_fds = {-1, -1, -1, -1},
    };
    slot->image = import_image(snapshotter, &destination);
    if (slot->image == EGL_NO_IMAGE_KHR) goto unsupported;
    glGenTextures(1, &slot->texture);
    glBindTexture(GL_TEXTURE_2D, slot->texture);
    snapshotter->bind_image(GL_TEXTURE_2D, slot->image);
    glGenFramebuffers(1, &slot->framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, slot->framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, slot->texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) goto unsupported;
    /* A complete framebuffer still must expose the requested precision. In
     * particular, never accept an implicit eight-bit attachment for RGB10. */
    GLint red_bits = 0, green_bits = 0, blue_bits = 0;
    glGetIntegerv(GL_RED_BITS, &red_bits);
    glGetIntegerv(GL_GREEN_BITS, &green_bits);
    glGetIntegerv(GL_BLUE_BITS, &blue_bits);
    GLint required_bits = precision == GBM_FORMAT_XRGB2101010 ? 10 : 8;
    if (red_bits < required_bits || green_bits < required_bits || blue_bits < required_bits)
        goto unsupported;
    if (glGetError() != GL_NO_ERROR) goto unsupported;
    *allocated = true;
    return AM_VKMS_SNAPSHOT_OK;
unsupported:
    release_slot(snapshotter, slot);
    return AM_VKMS_SNAPSHOT_UNSUPPORTED;
}

/* Requires the private context to be current. */
static void release_source(struct am_vkms_snapshotter *snapshotter, struct cached_source *source) {
    if (source->texture != 0) glDeleteTextures(1, &source->texture);
    if (source->image != EGL_NO_IMAGE_KHR)
        snapshotter->destroy_image(snapshotter->display, source->image);
    for (unsigned plane = 0; plane < 4; ++plane)
        if (source->fds[plane] >= 0) close(source->fds[plane]);
    memset(source, 0, sizeof(*source));
    for (unsigned plane = 0; plane < 4; ++plane) source->fds[plane] = -1;
}

static bool same_source(const struct cached_source *source,
                        const struct am_vkms_snapshot_input *input,
                        const struct stat *facts) {
    if (!source->used || source->width != input->width || source->height != input->height ||
        source->fourcc != input->fourcc || source->plane_count != input->plane_count ||
        source->modifier != input->modifier) return false;
    for (uint32_t plane = 0; plane < input->plane_count; ++plane)
        if (source->devices[plane] != facts[plane].st_dev ||
            source->inodes[plane] != facts[plane].st_ino ||
            source->pitches[plane] != input->pitches[plane] ||
            source->offsets[plane] != input->offsets[plane]) return false;
    return true;
}

/* Returns the cached import for this source identity and layout, importing
 * it once on a miss. The least recently used entry is evicted when full. */
static struct cached_source *source_import(struct am_vkms_snapshotter *snapshotter,
                                           const struct am_vkms_snapshot_input *input,
                                           bool *cached) {
    struct stat facts[4];
    *cached = false;
    for (uint32_t plane = 0; plane < input->plane_count; ++plane)
        if (fstat(input->fds[plane], &facts[plane]) != 0) return NULL;
    struct cached_source *victim = &snapshotter->sources[0];
    for (unsigned entry = 0; entry < SOURCE_CACHE_CAPACITY; ++entry) {
        struct cached_source *source = &snapshotter->sources[entry];
        if (same_source(source, input, facts)) {
            source->last_use = ++snapshotter->use_clock;
            *cached = true;
            return source;
        }
        if (victim->used && (!source->used || source->last_use < victim->last_use))
            victim = source;
    }
    release_source(snapshotter, victim);
    struct am_vkms_snapshot_input owned = *input;
    for (uint32_t plane = 0; plane < input->plane_count; ++plane) {
        victim->fds[plane] = fcntl(input->fds[plane], F_DUPFD_CLOEXEC, 0);
        if (victim->fds[plane] < 0) {
            release_source(snapshotter, victim);
            return NULL;
        }
        owned.fds[plane] = victim->fds[plane];
        victim->devices[plane] = facts[plane].st_dev;
        victim->inodes[plane] = facts[plane].st_ino;
        victim->pitches[plane] = input->pitches[plane];
        victim->offsets[plane] = input->offsets[plane];
    }
    victim->image = import_image(snapshotter, &owned);
    if (victim->image == EGL_NO_IMAGE_KHR) {
        release_source(snapshotter, victim);
        return NULL;
    }
    glGenTextures(1, &victim->texture);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, victim->texture);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    victim->used = true;
    victim->last_use = ++snapshotter->use_clock;
    victim->width = input->width;
    victim->height = input->height;
    victim->fourcc = input->fourcc;
    victim->plane_count = input->plane_count;
    victim->modifier = input->modifier;
    return victim;
}

/* Queues a GPU wait for every source acquire fence. EGL owns each duplicate
 * on successful sync creation. The borrowed originals stay with the caller,
 * which waits for them on the CPU when this returns false. */
static bool queue_acquire_waits(struct am_vkms_snapshotter *snapshotter,
                                const struct am_vkms_snapshot_input *input) {
    for (uint32_t plane = 0; plane < input->plane_count; ++plane) {
        if (input->acquire_fence_fds[plane] < 0) continue;
        if (!snapshotter->acquire_waits) return false;
        int duplicate = fcntl(input->acquire_fence_fds[plane], F_DUPFD_CLOEXEC, 0);
        if (duplicate < 0) return false;
        const EGLint attributes[] = {EGL_SYNC_NATIVE_FENCE_FD_ANDROID, duplicate, EGL_NONE};
        EGLSyncKHR sync = snapshotter->create_sync(snapshotter->display,
                                                   EGL_SYNC_NATIVE_FENCE_ANDROID, attributes);
        if (sync == EGL_NO_SYNC_KHR) {
            close(duplicate);
            return false;
        }
        EGLint waited = snapshotter->wait_sync(snapshotter->display, sync, 0);
        /* A destroyed sync stays alive while the queued wait needs it. */
        snapshotter->destroy_sync(snapshotter->display, sync);
        if (waited != EGL_TRUE) return false;
    }
    return true;
}

/* Keep the completion object until owner-thread polling proves all reads ended.
 * Failure leaves the pending copy quarantinable; it never falls back to glFinish. */
static int submit_copy(struct am_vkms_snapshotter *snapshotter) {
    snapshotter->pending_active = true;
    snapshotter->pending_completion = snapshotter->create_sync(snapshotter->display,
        EGL_SYNC_NATIVE_FENCE_ANDROID, NULL);
    if (snapshotter->pending_completion == EGL_NO_SYNC_KHR) return -1;
    glFlush();
    int fence = snapshotter->dup_native_fence(snapshotter->display, snapshotter->pending_completion);
    if (fence >= 0 && fcntl(fence, F_SETFD, FD_CLOEXEC) != 0) { close(fence); fence = -1; }
    return fence;
}

int am_vkms_snapshot_poll(struct am_vkms_snapshotter *snapshotter, uint64_t timeout_ns) {
    if (!snapshotter) return -2;
    if (!snapshotter->pending_active) return 1;
    if (snapshotter->pending_completion == EGL_NO_SYNC_KHR ||
        eglMakeCurrent(snapshotter->display, snapshotter->surface, snapshotter->surface,
                       snapshotter->context) != EGL_TRUE) return -2;
    EGLint result = snapshotter->client_wait_sync(snapshotter->display,
        snapshotter->pending_completion, 0, timeout_ns);
    if (result == EGL_TIMEOUT_EXPIRED_KHR) return 0;
    if (result != EGL_CONDITION_SATISFIED_KHR) return -2;
    snapshotter->destroy_sync(snapshotter->display, snapshotter->pending_completion);
    snapshotter->pending_completion = EGL_NO_SYNC_KHR;
    snapshotter->pending_active = false;
    return 1;
}

int am_vkms_snapshot_copy(struct am_vkms_snapshotter *snapshotter,
                          const struct am_vkms_snapshot_input *input,
                          struct am_vkms_snapshot_output *output) {
    if (snapshotter == NULL || input == NULL || output == NULL) return AM_VKMS_SNAPSHOT_UNSUPPORTED;
    output->fd = -1;
    output->fence_fd = -1;
    output->pitch = 0;
    output->fourcc = 0;
    output->modifier = DRM_FORMAT_MOD_INVALID;
    output->flags = 0;
    output->reserved = 0;
    if (snapshotter->pending_active) return AM_VKMS_SNAPSHOT_BACKEND_ERROR;
    if (input->width == 0 || input->height == 0 || input->width > 8192 || input->height > 8192 ||
        input->plane_count == 0 || input->plane_count > 4 || input->slot >= AM_VKMS_SNAPSHOT_SLOTS ||
        input->reserved != 0) return AM_VKMS_SNAPSHOT_UNSUPPORTED;
    uint32_t precision = precision_format(input->fourcc);
    if (precision == 0) return AM_VKMS_SNAPSHOT_UNSUPPORTED;
    for (uint32_t plane = 0; plane < input->plane_count; ++plane)
        if (input->fds[plane] < 0 || input->pitches[plane] == 0 ||
            input->pitches[plane] > INT32_MAX || input->offsets[plane] > INT32_MAX ||
            input->acquire_fence_fds[plane] < -1)
            return AM_VKMS_SNAPSHOT_UNSUPPORTED;
    if (eglMakeCurrent(snapshotter->display, snapshotter->surface, snapshotter->surface,
                       snapshotter->context) != EGL_TRUE) return AM_VKMS_SNAPSHOT_BACKEND_ERROR;
    /* This private context has no external state. A lost context must fail
     * rather than loop indefinitely while clearing its error flag. */
    if (glGetError() != GL_NO_ERROR) return AM_VKMS_SNAPSHOT_BACKEND_ERROR;
    if (!queue_acquire_waits(snapshotter, input)) return AM_VKMS_SNAPSHOT_ACQUIRE_NOT_QUEUED;
    bool cached = false;
    struct cached_source *source = source_import(snapshotter, input, &cached);
    if (source == NULL) return AM_VKMS_SNAPSHOT_UNSUPPORTED;
    struct snapshot_slot *slot = &snapshotter->slots[input->slot];
    bool allocated = false;
    if (prepare_slot(snapshotter, slot, input->width, input->height, precision, &allocated) !=
        AM_VKMS_SNAPSHOT_OK) return AM_VKMS_SNAPSHOT_UNSUPPORTED;
    int destination_fd = fcntl(slot->fd, F_DUPFD_CLOEXEC, 0);
    if (destination_fd < 0) return AM_VKMS_SNAPSHOT_BACKEND_ERROR;
    static const GLfloat positions[] = {-1,-1, 1,-1, -1,1, 1,1};
    static const GLfloat coordinates[] = {0,0, 1,0, 0,1, 1,1};
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, source->texture);
    /* Re-specify the binding so drivers that resolve an external image at
     * bind time sample this presentation's contents, not a cached copy. */
    snapshotter->bind_image(GL_TEXTURE_EXTERNAL_OES, source->image);
    glBindFramebuffer(GL_FRAMEBUFFER, slot->framebuffer);
    glViewport(0, 0, (GLsizei)input->width, (GLsizei)input->height);
    glDisable(GL_BLEND);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_DITHER);
    glUseProgram(snapshotter->program);
    glUniform1i(snapshotter->sampler, 0);
    glVertexAttribPointer((GLuint)snapshotter->position, 2, GL_FLOAT, GL_FALSE, 0, positions);
    glVertexAttribPointer((GLuint)snapshotter->coordinates, 2, GL_FLOAT, GL_FALSE, 0, coordinates);
    glEnableVertexAttribArray((GLuint)snapshotter->position);
    glEnableVertexAttribArray((GLuint)snapshotter->coordinates);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    int fence = submit_copy(snapshotter);
    if (glGetError() != GL_NO_ERROR || fence < 0) {
        if (fence >= 0) close(fence);
        close(destination_fd);
        return AM_VKMS_SNAPSHOT_BACKEND_ERROR;
    }
    output->fd = destination_fd;
    output->fence_fd = fence;
    output->pitch = slot->pitch;
    output->fourcc = slot->fourcc;
    output->modifier = slot->modifier;
    output->flags = (cached ? AM_VKMS_SNAPSHOT_SOURCE_CACHED : 0u) |
                    (allocated ? AM_VKMS_SNAPSHOT_SLOT_ALLOCATED : 0u);
    return AM_VKMS_SNAPSHOT_OK;
}

void am_vkms_snapshot_destroy(struct am_vkms_snapshotter *snapshotter) {
    if (snapshotter == NULL) return;
    if (snapshotter->pending_active && am_vkms_snapshot_poll(snapshotter, 1000000000u) != 1) return;
    if (snapshotter->display != EGL_NO_DISPLAY && snapshotter->context != EGL_NO_CONTEXT &&
        snapshotter->surface != EGL_NO_SURFACE &&
        eglMakeCurrent(snapshotter->display, snapshotter->surface, snapshotter->surface,
                       snapshotter->context) == EGL_TRUE) {
        /* Submitted copies complete before their GPU objects are released.
         * Exported descriptors keep each private allocation alive. */
        for (unsigned slot = 0; slot < AM_VKMS_SNAPSHOT_SLOTS; ++slot)
            release_slot(snapshotter, &snapshotter->slots[slot]);
        for (unsigned entry = 0; entry < SOURCE_CACHE_CAPACITY; ++entry)
            release_source(snapshotter, &snapshotter->sources[entry]);
        if (snapshotter->program != 0) glDeleteProgram(snapshotter->program);
        (void)eglMakeCurrent(snapshotter->display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    } else {
        /* Without a current context only descriptors can be released. */
        for (unsigned slot = 0; slot < AM_VKMS_SNAPSHOT_SLOTS; ++slot) {
            if (snapshotter->slots[slot].fd >= 0) close(snapshotter->slots[slot].fd);
            if (snapshotter->slots[slot].buffer != NULL) gbm_bo_destroy(snapshotter->slots[slot].buffer);
        }
        for (unsigned entry = 0; entry < SOURCE_CACHE_CAPACITY; ++entry)
            for (unsigned plane = 0; plane < 4; ++plane)
                if (snapshotter->sources[entry].fds[plane] >= 0)
                    close(snapshotter->sources[entry].fds[plane]);
    }
    if (snapshotter->display != EGL_NO_DISPLAY) {
        if (snapshotter->context != EGL_NO_CONTEXT) eglDestroyContext(snapshotter->display, snapshotter->context);
        if (snapshotter->surface != EGL_NO_SURFACE) eglDestroySurface(snapshotter->display, snapshotter->surface);
        eglTerminate(snapshotter->display);
    }
    if (snapshotter->gbm != NULL) gbm_device_destroy(snapshotter->gbm);
    if (snapshotter->render_fd >= 0) close(snapshotter->render_fd);
    free(snapshotter);
}
