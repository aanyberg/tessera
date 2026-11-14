/*
 * Plugins JSON smoke test of TesseraCore.h from C (#48, ADR 0047), built
 * and run by scripts/ffi-c-test.sh: with a config.toml that enables Hello,
 * read the plugins JSON with the two-call protocol until Hello is active
 * with its status item, see a buffer one byte short refused with the
 * length and nothing written, and check the NULL arguments. Uses a
 * configuration directory of its own under the script's
 * TESSERA_CONFIG_DIR, so later tests start from no config.toml.
 */

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

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

/* The plugins JSON, NUL-terminated; NULL on failure. The plugin thread may
 * activate Hello between the two calls, so a refusal with a longer length
 * asks again. */
static char *plugins_json(const TesseraApp *app) {
    size_t len = 0;
    if (tessera_app_plugins_json(app, NULL, 0, &len) != TESSERA_ERROR_INVALID_ARGUMENT ||
        len == 0) {
        return NULL;
    }
    for (int attempt = 0; attempt < 100; attempt++) {
        char *json = malloc(len + 1);
        if (json == NULL) {
            return NULL;
        }
        size_t written = 0;
        TesseraError status = tessera_app_plugins_json(app, (uint8_t *)json, len, &written);
        if (status == TESSERA_ERROR_OK && written <= len) {
            json[written] = 0;
            return json;
        }
        free(json);
        if (status != TESSERA_ERROR_INVALID_ARGUMENT || written <= len) {
            return NULL;
        }
        len = written;
    }
    return NULL;
}

int main(void) {
    const char *base = getenv("TESSERA_CONFIG_DIR");
    CHECK(base != NULL && base[0] == '/');
    char dir[1024];
    snprintf(dir, sizeof dir, "%s/plugins-c-test", base);
    CHECK(mkdir(dir, 0700) == 0);
    CHECK(setenv("TESSERA_CONFIG_DIR", dir, 1) == 0);
    char config_path[1100];
    snprintf(config_path, sizeof config_path, "%s/config.toml", dir);
    FILE *config = fopen(config_path, "w");
    CHECK(config != NULL);
    CHECK(fputs("[plugins]\nenabled = [\"dev.tessera.hello\"]\n", config) >= 0);
    CHECK(fclose(config) == 0);

    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    /* Hello activates on the plugin thread: poll for at most 30 s. */
    char *json = NULL;
    for (int attempt = 0; attempt < 3000; attempt++) {
        free(json);
        json = plugins_json(app);
        CHECK(json != NULL);
        if (strstr(json, "\"state\":\"active\"") != NULL &&
            strstr(json, "\"text\":\"0 sessions\"") != NULL) {
            break;
        }
        usleep(10000);
    }
    CHECK(json[0] == '[');
    CHECK(strstr(json, "\"id\":\"dev.tessera.hello\"") != NULL);
    CHECK(strstr(json, "\"name\":\"Hello\"") != NULL);
    CHECK(strstr(json, "\"enabled\":true") != NULL);
    CHECK(strstr(json, "\"state\":\"active\"") != NULL);
    CHECK(strstr(json, "\"capabilities\":[\"sessions\",\"status_bar\"]") != NULL);
    CHECK(strstr(json, "\"message\":null") != NULL);
    CHECK(strstr(json, "\"text\":\"0 sessions\"") != NULL);

    /* One byte short: refused with the length, nothing written. */
    size_t len = strlen(json);
    char *small = malloc(len);
    CHECK(small != NULL);
    memset(small, '#', len);
    size_t got = 0;
    EXPECT(tessera_app_plugins_json(app, (uint8_t *)small, len - 1, &got),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(got == len);
    for (size_t i = 0; i < len; i++) {
        CHECK(small[i] == '#');
    }
    free(small);
    free(json);

    /* Argument checks. */
    got = 99;
    EXPECT(tessera_app_plugins_json(NULL, NULL, 0, &got), TESSERA_ERROR_NULL_POINTER);
    CHECK(got == 0);
    EXPECT(tessera_app_plugins_json(app, NULL, 0, NULL), TESSERA_ERROR_NULL_POINTER);

    tessera_app_free(app);
    puts("plugins: ok");
    return 0;
}
