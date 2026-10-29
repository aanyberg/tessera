/*
 * Surface smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: create a bare CAMetalLayer through the
 * Objective-C runtime, attach it to a /bin/sh session at 800x600 scale 1,
 * render 3 frames, resize to 1600x1200 scale 2, render 3 frames and read
 * the surface's metrics, resize to 0x0 (frames are skipped), back to
 * 800x600, detach (metrics then report no surface) and close.
 *
 * A layer outside any window may not vend drawables, so each frame may
 * report PRESENTED or SKIPPED; the test prints which. Skipped entirely when
 * TESSERA_SKIP_GPU_TESTS is set, like the render suites.
 */

#include <objc/message.h>
#include <objc/runtime.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>

#include "TesseraCore.h"

/* MTLCreateSystemDefaultDevice returns id<MTLDevice>; an object pointer. */
extern void *MTLCreateSystemDefaultDevice(void);

/* CGSize, laid out as in CoreGraphics (two CGFloats, doubles on 64-bit). */
struct size {
    double width;
    double height;
};

/* The layout the Rust side pins (surface::tests); a drift breaks the build. */
_Static_assert(sizeof(TesseraSurfaceMetrics) == 44, "TesseraSurfaceMetrics size");
_Static_assert(offsetof(TesseraSurfaceMetrics, font_size_pt) == 32, "font_size_pt offset");
_Static_assert(offsetof(TesseraSurfaceMetrics, cursor_wide) == 41, "cursor_wide offset");

typedef id (*send_id_fn)(id, SEL);
typedef void (*send_obj_fn)(id, SEL, void *);
typedef void (*send_size_fn)(id, SEL, struct size);

#define CHECK(expr)                                                              \
    do {                                                                         \
        TesseraError check_status = (expr);                                      \
        if (check_status != TESSERA_ERROR_OK) {                                  \
            fprintf(stderr, "%s:%d: %s returned %d\n", __FILE__, __LINE__, #expr, \
                    (int)check_status);                                          \
            return 1;                                                            \
        }                                                                        \
    } while (0)

static const char *status_name(TesseraFrameStatus status) {
    switch (status) {
    case TESSERA_FRAME_STATUS_PRESENTED: return "presented";
    case TESSERA_FRAME_STATUS_SUBOPTIMAL: return "suboptimal";
    case TESSERA_FRAME_STATUS_SKIPPED: return "skipped";
    }
    return "unknown";
}

/* Renders `frames` frames; each must succeed and be presented or skipped. */
static int render(TesseraApp *app, uint64_t id, int frames, const char *label) {
    printf("%s:", label);
    for (int i = 0; i < frames; i++) {
        TesseraFrameStatus status = TESSERA_FRAME_STATUS_PRESENTED;
        CHECK(tessera_surface_render(app, id, &status));
        if (status != TESSERA_FRAME_STATUS_PRESENTED && status != TESSERA_FRAME_STATUS_SKIPPED) {
            fprintf(stderr, "\nframe %d of %s: unexpected status %s\n", i, label,
                    status_name(status));
            return 1;
        }
        printf(" %s", status_name(status));
    }
    printf("\n");
    return 0;
}

int main(void) {
    if (getenv("TESSERA_SKIP_GPU_TESTS") != NULL) {
        printf("TESSERA_SKIP_GPU_TESTS is set; skipping\n");
        return 0;
    }

    send_id_fn send_id = (send_id_fn)objc_msgSend;
    send_obj_fn send_obj = (send_obj_fn)objc_msgSend;
    send_size_fn send_size = (send_size_fn)objc_msgSend;

    Class layer_class = objc_getClass("CAMetalLayer");
    if (layer_class == NULL) {
        fprintf(stderr, "CAMetalLayer is not registered (QuartzCore not linked?)\n");
        return 1;
    }
    /* Autoreleased; the process's main-thread pool keeps it alive. */
    id layer = send_id((id)layer_class, sel_registerName("layer"));
    void *device = MTLCreateSystemDefaultDevice();
    if (layer == NULL || device == NULL) {
        fprintf(stderr, "no CAMetalLayer (%p) or Metal device (%p)\n", (void *)layer, device);
        return 1;
    }
    send_obj(layer, sel_registerName("setDevice:"), device);
    send_size(layer, sel_registerName("setDrawableSize:"), (struct size){800.0, 600.0});

    TesseraApp *app = NULL;
    CHECK(tessera_app_new(&app));
    TesseraSpawnOptions options = {
        .program = "/bin/sh",
        .args = NULL,
        .arg_count = 0,
        .cwd = NULL,
        .rows = 24,
        .cols = 80,
        .width_px = 0,
        .height_px = 0,
    };
    uint64_t id = 0;
    CHECK(tessera_session_spawn(app, &options, &id));

    CHECK(tessera_surface_attach(app, id, (void *)layer, 800, 600, 1.0f));
    if (render(app, id, 3, "800x600 @1x") != 0) {
        return 1;
    }
    CHECK(tessera_surface_resize(app, id, 1600, 1200, 2.0f));
    if (render(app, id, 3, "1600x1200 @2x") != 0) {
        return 1;
    }

    TesseraSurfaceMetrics metrics = {0};
    CHECK(tessera_surface_metrics(app, id, &metrics));
    printf("metrics: %ux%u @%gx, cell %ux%u, padding %u, %ux%u cells, %g pt, cursor %u,%u\n",
           metrics.width_px, metrics.height_px, (double)metrics.scale, metrics.cell_width_px,
           metrics.cell_height_px, metrics.padding_px, (unsigned)metrics.cols,
           (unsigned)metrics.rows, (double)metrics.font_size_pt, (unsigned)metrics.cursor_row,
           (unsigned)metrics.cursor_col);
    if (metrics.width_px != 1600 || metrics.height_px != 1200 || metrics.scale != 2.0f ||
        metrics.cell_width_px == 0 || metrics.rows == 0 || metrics.cols == 0 ||
        metrics.cursor_row >= metrics.rows || metrics.cursor_col >= metrics.cols) {
        fprintf(stderr, "unexpected metrics\n");
        return 1;
    }

    CHECK(tessera_surface_resize(app, id, 0, 0, 2.0f));
    TesseraFrameStatus status = TESSERA_FRAME_STATUS_PRESENTED;
    CHECK(tessera_surface_render(app, id, &status));
    if (status != TESSERA_FRAME_STATUS_SKIPPED) {
        fprintf(stderr, "a 0x0 surface should skip frames, got %s\n", status_name(status));
        return 1;
    }
    printf("0x0: %s\n", status_name(status));

    CHECK(tessera_surface_resize(app, id, 800, 600, 1.0f));
    if (render(app, id, 1, "800x600 @1x again") != 0) {
        return 1;
    }
    CHECK(tessera_surface_detach(app, id));
    if (tessera_surface_detach(app, id) != TESSERA_ERROR_NO_SURFACE) {
        fprintf(stderr, "a second detach should report TESSERA_ERROR_NO_SURFACE\n");
        return 1;
    }
    if (tessera_surface_metrics(app, id, &metrics) != TESSERA_ERROR_NO_SURFACE) {
        fprintf(stderr, "metrics without a surface should report TESSERA_ERROR_NO_SURFACE\n");
        return 1;
    }
    CHECK(tessera_session_close(app, id));
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
