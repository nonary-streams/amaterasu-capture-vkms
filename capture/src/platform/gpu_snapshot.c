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

struct am_vkms_snapshotter {
    int render_fd;
    struct gbm_device *gbm;
    EGLDisplay display;
    EGLContext context;
    EGLSurface surface;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
    GLuint program;
    GLint position;
    GLint coordinates;
    GLint sampler;
    bool modifiers;
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
    snapshotter->surface = eglCreatePbufferSurface(snapshotter->display, config, surface);
    snapshotter->context = eglCreateContext(snapshotter->display, config, EGL_NO_CONTEXT, context);
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

int am_vkms_snapshot_copy(struct am_vkms_snapshotter *snapshotter,
                          const struct am_vkms_snapshot_input *input,
                          struct am_vkms_snapshot_output *output) {
    if (snapshotter == NULL || input == NULL || output == NULL ||
        input->width == 0 || input->height == 0 || input->width > 8192 || input->height > 8192 ||
        input->plane_count == 0 || input->plane_count > 4) return -1;
    output->fd = -1;
    output->pitch = 0;
    output->fourcc = 0;
    output->reserved = 0;
    output->modifier = DRM_FORMAT_MOD_INVALID;
    /* Keep the source's RGB precision. Ten-bit storage does not imply HDR:
     * transfer function and primaries remain the caller's separate contract.
     * Never fall back from a ten-bit source to an eight-bit destination. */
    uint32_t destination_format;
    switch (input->fourcc) {
    case DRM_FORMAT_XRGB8888:
    case DRM_FORMAT_ARGB8888:
    case DRM_FORMAT_XBGR8888:
    case DRM_FORMAT_ABGR8888:
        destination_format = GBM_FORMAT_XRGB8888;
        break;
    case DRM_FORMAT_XRGB2101010:
    case DRM_FORMAT_ARGB2101010:
    case DRM_FORMAT_XBGR2101010:
    case DRM_FORMAT_ABGR2101010:
        destination_format = GBM_FORMAT_XRGB2101010;
        break;
    default:
        /* Do not guess the matrix/range metadata of YUV sources. */
        return -1;
    }
    for (uint32_t plane = 0; plane < input->plane_count; ++plane)
        if (input->fds[plane] < 0 || input->pitches[plane] == 0 ||
            input->pitches[plane] > INT32_MAX || input->offsets[plane] > INT32_MAX) return -1;
    if (eglMakeCurrent(snapshotter->display, snapshotter->surface, snapshotter->surface,
                       snapshotter->context) != EGL_TRUE) return -2;
    /* This private context has no external state. A lost context must fail
     * rather than loop indefinitely while clearing its error flag. */
    if (glGetError() != GL_NO_ERROR) return -2;
    EGLImageKHR source = import_image(snapshotter, input);
    if (source == EGL_NO_IMAGE_KHR) return -1;
    struct gbm_bo *buffer = allocate_snapshot(snapshotter->gbm, input->width,
                                              input->height, &destination_format);
    GLuint source_texture = 0, destination_texture = 0, framebuffer = 0;
    EGLImageKHR destination = EGL_NO_IMAGE_KHR;
    int result = -1;
    int destination_fd = -1;
    if (buffer == NULL || gbm_bo_get_plane_count(buffer) != 1) goto cleanup;
    destination_fd = gbm_bo_get_fd(buffer);
    if (destination_fd < 0 || fcntl(destination_fd, F_SETFD, FD_CLOEXEC) != 0) goto cleanup;
    struct am_vkms_snapshot_input destination_input = {
        .width = input->width, .height = input->height, .fourcc = destination_format,
        .plane_count = 1, .modifier = gbm_bo_get_modifier(buffer),
        .fds = {destination_fd, -1, -1, -1},
        .pitches = {gbm_bo_get_stride(buffer), 0, 0, 0},
        .offsets = {gbm_bo_get_offset(buffer, 0), 0, 0, 0},
    };
    /* This implementation only exports a single-plane buffer beginning at zero. */
    if (destination_input.offsets[0] != 0) goto cleanup;
    destination = import_image(snapshotter, &destination_input);
    if (destination == EGL_NO_IMAGE_KHR) goto cleanup;
    glGenTextures(1, &source_texture);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, source_texture);
    snapshotter->bind_image(GL_TEXTURE_EXTERNAL_OES, source);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glGenTextures(1, &destination_texture);
    glBindTexture(GL_TEXTURE_2D, destination_texture);
    snapshotter->bind_image(GL_TEXTURE_2D, destination);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, destination_texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) goto cleanup;
    /* A complete framebuffer still must expose the requested precision. In
     * particular, never accept an implicit eight-bit attachment for RGB10. */
    GLint red_bits = 0, green_bits = 0, blue_bits = 0;
    glGetIntegerv(GL_RED_BITS, &red_bits);
    glGetIntegerv(GL_GREEN_BITS, &green_bits);
    glGetIntegerv(GL_BLUE_BITS, &blue_bits);
    GLint required_bits = (input->fourcc == DRM_FORMAT_XRGB2101010 || input->fourcc == DRM_FORMAT_ARGB2101010 ||
                           input->fourcc == DRM_FORMAT_XBGR2101010 || input->fourcc == DRM_FORMAT_ABGR2101010) ? 10 : 8;
    if (red_bits < required_bits || green_bits < required_bits || blue_bits < required_bits)
        goto cleanup;
    static const GLfloat positions[] = {-1,-1, 1,-1, -1,1, 1,1};
    static const GLfloat coordinates[] = {0,0, 1,0, 0,1, 1,1};
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
    /* A completed private allocation has no producer that can overwrite it.
     * The exported DMA-BUF owns storage beyond deletion of its temporary BO. */
    glFinish();
    if (glGetError() != GL_NO_ERROR) { result = -2; goto cleanup; }
    output->fd = destination_fd;
    output->pitch = destination_input.pitches[0];
    output->fourcc = destination_input.fourcc;
    output->modifier = destination_input.modifier;
    destination_fd = -1;
    result = 0;
cleanup:
    if (framebuffer != 0) glDeleteFramebuffers(1, &framebuffer);
    if (source_texture != 0) glDeleteTextures(1, &source_texture);
    if (destination_texture != 0) glDeleteTextures(1, &destination_texture);
    if (destination != EGL_NO_IMAGE_KHR) snapshotter->destroy_image(snapshotter->display, destination);
    snapshotter->destroy_image(snapshotter->display, source);
    if (destination_fd >= 0) close(destination_fd);
    if (buffer != NULL) gbm_bo_destroy(buffer);
    return result;
}

void am_vkms_snapshot_destroy(struct am_vkms_snapshotter *snapshotter) {
    if (snapshotter == NULL) return;
    if (snapshotter->display != EGL_NO_DISPLAY && snapshotter->context != EGL_NO_CONTEXT &&
        snapshotter->surface != EGL_NO_SURFACE) {
        (void)eglMakeCurrent(snapshotter->display, snapshotter->surface, snapshotter->surface,
                             snapshotter->context);
        if (snapshotter->program != 0) glDeleteProgram(snapshotter->program);
        (void)eglMakeCurrent(snapshotter->display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
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
