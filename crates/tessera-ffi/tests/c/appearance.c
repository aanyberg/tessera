/*
 * Window appearance smoke test of TesseraCore.h from C (#388, #390,
 * ADR 0045), built and run by scripts/ffi-c-test.sh: the layout of
 * TesseraSessionAppearance with its TesseraRgba background, a session's
 * configured background colour and alpha, the opaque-black failure default,
 * tessera_app_blur_radius with its clamp and argument checks, and
 * tessera_app_app_appearance with its named values (#365). Uses a
 * configuration directory of its own under the script's TESSERA_CONFIG_DIR,
 * so later tests start from no config.toml.
 */

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "TesseraCore.h"

#define EXPECT(expr, status)                                                         \
    do {                                                                             \
        TesseraError expect_status = (expr);                                         \
        if (expect_status != (status)) {                                             \
            fprintf(stderr, "%s:%d: %s returned %d, expected %d\n", __FILE__, __LINE__, \
                    #expr, (int)expect_status, (int)(status));                       \
            return 1;                                                                \
        }                                                                            \
    } while (0)

#define CHECK(cond)                                                                  \
    do {                                                                             \
        if (!(cond)) {                                                               \
            fprintf(stderr, "%s:%d: %s does not hold\n", __FILE__, __LINE__, #cond); \
            return 1;                                                                \
        }                                                                            \
    } while (0)

/* Dracula's background, #282a36, at the configured opacity. */
static const char CONFIG[] = "[appearance]\n"
                             "theme = \"Dracula\"\n"
                             "opacity = 0.5\n"
                             "blur = true\n"
                             "blur_radius = 100\n"
                             "app_appearance = \"system\"\n";

/* What a failed call must overwrite. */
static const TesseraSessionAppearance GARBAGE = {
    .opaque = false,
    .blur = true,
    .background = {.r = 0.25f, .g = 0.5f, .b = 0.75f, .a = 0.125f},
};

static int is_opaque_black(TesseraRgba c) {
    return c.r == 0.0f && c.g == 0.0f && c.b == 0.0f && c.a == 1.0f;
}

int main(void) {
    /* The layout Swift and C read: two flags, then four floats. */
    CHECK(sizeof(TesseraRgba) == 16);
    CHECK(offsetof(TesseraRgba, a) == 12);
    CHECK(offsetof(TesseraSessionAppearance, opaque) == 0);
    CHECK(offsetof(TesseraSessionAppearance, blur) == 1);
    CHECK(offsetof(TesseraSessionAppearance, background) == 4);
    CHECK(sizeof(TesseraSessionAppearance) == 20);

    const char *base = getenv("TESSERA_CONFIG_DIR");
    CHECK(base != NULL && base[0] == '/');
    char dir[1024];
    snprintf(dir, sizeof dir, "%s/appearance-c-test", base);
    CHECK(mkdir(dir, 0700) == 0);
    CHECK(setenv("TESSERA_CONFIG_DIR", dir, 1) == 0);
    char config_path[1100];
    snprintf(config_path, sizeof config_path, "%s/config.toml", dir);
    FILE *file = fopen(config_path, "wb");
    CHECK(file != NULL);
    CHECK(fwrite(CONFIG, 1, strlen(CONFIG), file) == strlen(CONFIG));
    CHECK(fclose(file) == 0);

    /* Argument checks: the output is zeroed before the handle is looked at. */
    uint32_t radius = 7;
    EXPECT(tessera_app_blur_radius(NULL, &radius), TESSERA_ERROR_NULL_POINTER);
    CHECK(radius == 0);
    TesseraSessionAppearance look = GARBAGE;
    EXPECT(tessera_session_appearance(NULL, 1, &look), TESSERA_ERROR_NULL_POINTER);
    CHECK(look.opaque && !look.blur && is_opaque_black(look.background));

    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);
    EXPECT(tessera_app_blur_radius(app, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_appearance(app, 1, NULL), TESSERA_ERROR_NULL_POINTER);

    /* The header's names for the app's appearance, and their values. */
    CHECK(TESSERA_APP_APPEARANCE_DARK == 0);
    CHECK(TESSERA_APP_APPEARANCE_LIGHT == 1);
    CHECK(TESSERA_APP_APPEARANCE_SYSTEM == 2);
    uint32_t app_appearance = 7;
    EXPECT(tessera_app_app_appearance(NULL, &app_appearance), TESSERA_ERROR_NULL_POINTER);
    CHECK(app_appearance == TESSERA_APP_APPEARANCE_DARK);
    EXPECT(tessera_app_app_appearance(app, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_app_app_appearance(app, &app_appearance), TESSERA_ERROR_OK);
    CHECK(app_appearance == TESSERA_APP_APPEARANCE_SYSTEM);

    /* 100 is past the bound: the file loads with it lowered to 64. */
    radius = 7;
    EXPECT(tessera_app_blur_radius(app, &radius), TESSERA_ERROR_OK);
    CHECK(radius == 64);

    look = GARBAGE;
    EXPECT(tessera_session_appearance(app, 9999, &look), TESSERA_ERROR_SESSION_NOT_FOUND);
    CHECK(look.opaque && !look.blur && is_opaque_black(look.background));

    static const char *const args[] = {"-c", "sleep 30"};
    TesseraSpawnOptions options = {
        .program = "/bin/sh",
        .args = args,
        .arg_count = 2,
        .cwd = NULL,
        .rows = 24,
        .cols = 80,
        .width_px = 0,
        .height_px = 0,
        .profile = NULL,
    };
    uint64_t id = 0;
    EXPECT(tessera_session_spawn(app, &options, &id), TESSERA_ERROR_OK);
    look = GARBAGE;
    EXPECT(tessera_session_appearance(app, id, &look), TESSERA_ERROR_OK);
    CHECK(!look.opaque && look.blur);
    CHECK(look.background.r == 0x28 / 255.0f);
    CHECK(look.background.g == 0x2a / 255.0f);
    CHECK(look.background.b == 0x36 / 255.0f);
    CHECK(look.background.a == 0.5f);

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    tessera_app_free(app);
    puts("appearance ok");
    return 0;
}
