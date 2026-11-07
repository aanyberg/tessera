/*
 * Settings write-back smoke test of TesseraCore.h from C (#40, ADR 0040),
 * built and run by scripts/ffi-c-test.sh: read the configuration JSON with
 * the two-call protocol, set and remove a setting by key path (the file
 * keeps its comments), see a refused value come back with a message and
 * leave the file alone, check the argument checks, list the themes and
 * import one. Uses a configuration directory of its own under the
 * script's TESSERA_CONFIG_DIR, so later tests start from no config.toml.
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

#define S(text) (const uint8_t *)(text), strlen(text)

/* Reads a whole file into a NUL-terminated buffer the caller frees. */
static char *slurp(const char *path) {
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        return NULL;
    }
    static char buf[1 << 16];
    size_t n = fread(buf, 1, sizeof buf - 1, file);
    fclose(file);
    buf[n] = 0;
    char *copy = malloc(n + 1);
    if (copy != NULL) {
        memcpy(copy, buf, n + 1);
    }
    return copy;
}

/* JSON from one of the buffer exports, NUL-terminated; NULL on failure. */
static char *json_of(TesseraError (*f)(const TesseraApp *, uint8_t *, size_t, size_t *),
                     const TesseraApp *app) {
    size_t len = 0;
    if (f(app, NULL, 0, &len) != TESSERA_ERROR_INVALID_ARGUMENT || len == 0) {
        return NULL;
    }
    char *json = malloc(len + 1);
    size_t written = 0;
    if (json == NULL || f(app, (uint8_t *)json, len, &written) != TESSERA_ERROR_OK ||
        written != len) {
        free(json);
        return NULL;
    }
    json[len] = 0;
    return json;
}

static const char BASE16[] =
    "scheme: \"Ocean\"\nauthor: \"x\"\n"
    "base00: \"2b303b\"\nbase01: \"343d46\"\nbase02: \"4f5b66\"\nbase03: \"65737e\"\n"
    "base04: \"a7adba\"\nbase05: \"c0c5ce\"\nbase06: \"dfe1e8\"\nbase07: \"eff1f5\"\n"
    "base08: \"bf616a\"\nbase09: \"d08770\"\nbase0A: \"ebcb8b\"\nbase0B: \"a3be8c\"\n"
    "base0C: \"96b5b4\"\nbase0D: \"8fa1b3\"\nbase0E: \"b48ead\"\nbase0F: \"ab7967\"\n";

int main(void) {
    const char *base = getenv("TESSERA_CONFIG_DIR");
    CHECK(base != NULL && base[0] == '/');
    char dir[1024];
    snprintf(dir, sizeof dir, "%s/config-c-test", base);
    CHECK(mkdir(dir, 0700) == 0);
    CHECK(setenv("TESSERA_CONFIG_DIR", dir, 1) == 0);
    char config_path[1100];
    snprintf(config_path, sizeof config_path, "%s/config.toml", dir);

    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    /* The configuration in effect: the defaults, generation 0. */
    char *json = json_of(tessera_config_json, app);
    CHECK(json != NULL);
    CHECK(strstr(json, "\"generation\":0") != NULL);
    CHECK(strstr(json, "\"opacity\":1.0") != NULL);
    free(json);
    size_t len = 0;
    EXPECT(tessera_config_json(NULL, NULL, 0, &len), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_config_json(app, NULL, 0, NULL), TESSERA_ERROR_NULL_POINTER);

    /* A set creates the commented default file and edits it in place. */
    uint8_t msg[256];
    size_t msg_len = 99;
    EXPECT(tessera_config_set(app, S("appearance.opacity"), S("0.5"), msg, sizeof msg, &msg_len),
           TESSERA_ERROR_OK);
    CHECK(msg_len == 0);
    char *text = slurp(config_path);
    CHECK(text != NULL);
    CHECK(strstr(text, "\nopacity = 0.5\n") != NULL);
    CHECK(strstr(text, "# Window opacity from 0.0") != NULL);
    free(text);

    /* Refused: out of range, unknown key, missing theme, unknown command. */
    const char *refused[][2] = {
        {"appearance.opacity", "5.0"},
        {"appearance.opacityy", "0.5"},
        {"appearance.theme", "\"No Such Theme\""},
        {"appearance.opacity", "\"opaque\""},
        {"keys.\"cmd+e\"", "\"no_such_command\""},
    };
    for (size_t i = 0; i < sizeof refused / sizeof refused[0]; i++) {
        msg_len = 0;
        EXPECT(tessera_config_set(app, S(refused[i][0]), S(refused[i][1]), msg, sizeof msg,
                                  &msg_len),
               TESSERA_ERROR_REFUSED);
        CHECK(msg_len > 0 && msg_len <= sizeof msg);
    }
    /* A message cut to the buffer, on a character boundary. */
    EXPECT(tessera_config_set(app, S("appearance.opacity"), S("5.0"), msg, 4, &msg_len),
           TESSERA_ERROR_REFUSED);
    CHECK(msg_len <= 4);
    text = slurp(config_path);
    CHECK(text != NULL && strstr(text, "\nopacity = 0.5\n") != NULL);
    CHECK(strstr(text, "opacityy") == NULL && strstr(text, "no_such_command") == NULL);
    free(text);

    /* Argument checks. */
    EXPECT(tessera_config_set(NULL, S("appearance.opacity"), S("0.5"), NULL, 0, NULL),
           TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_config_set(app, NULL, 3, S("0.5"), NULL, 0, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_config_set(app, S("appearance.opacity"), NULL, 3, NULL, 0, NULL),
           TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_config_set(app, S("appearance..opacity"), S("0.5"), NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT(tessera_config_set(app, S("appearance.opacity"), S("null"), NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT(tessera_config_set(app, S("appearance"), S("1"), NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT(tessera_config_set(app, (const uint8_t *)"\xff", 1, S("1"), NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    static char long_key[2048];
    memset(long_key, 'a', sizeof long_key - 1);
    EXPECT(tessera_config_set(app, S(long_key), S("1"), NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT(tessera_config_set(app, S("appearance.opacity"), (const uint8_t *)"1",
                              TESSERA_MAX_CONFIG_VALUE_LEN + 1, NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);

    /* Remove: the default applies again, the comment stays. */
    EXPECT(tessera_config_remove(app, S("appearance.opacity"), msg, sizeof msg, &msg_len),
           TESSERA_ERROR_OK);
    EXPECT(tessera_config_remove(app, S("appearance.opacity"), NULL, 0, NULL), TESSERA_ERROR_OK);
    EXPECT(tessera_config_remove(NULL, S("appearance.opacity"), NULL, 0, NULL),
           TESSERA_ERROR_NULL_POINTER);
    text = slurp(config_path);
    CHECK(text != NULL && strstr(text, "\nopacity =") == NULL);
    CHECK(strstr(text, "# Window opacity from 0.0") != NULL);
    free(text);

    /* Themes: the bundled ones, then an imported one. */
    json = json_of(tessera_theme_list, app);
    CHECK(json != NULL);
    CHECK(strstr(json, "{\"id\":\"Tessera Dark\",\"name\":\"Tessera Dark\","
                       "\"source\":\"bundled\"}") != NULL);
    free(json);
    char scheme[1100];
    snprintf(scheme, sizeof scheme, "%s/ocean.yaml", base);
    FILE *file = fopen(scheme, "wb");
    CHECK(file != NULL);
    CHECK(fwrite(BASE16, 1, sizeof BASE16 - 1, file) == sizeof BASE16 - 1);
    fclose(file);
    uint8_t id[TESSERA_THEME_ID_LEN];
    size_t id_len = 99;
    EXPECT(tessera_theme_import(app, S(scheme), id, TESSERA_THEME_ID_LEN - 1, &id_len, msg,
                                sizeof msg, &msg_len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(id_len == 0);
    EXPECT(tessera_theme_import(app, S(scheme), NULL, sizeof id, &id_len, NULL, 0, NULL),
           TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_theme_import(app, S(scheme), id, sizeof id, &id_len, msg, sizeof msg, &msg_len),
           TESSERA_ERROR_OK);
    CHECK(id_len == 5 && memcmp(id, "ocean", 5) == 0 && msg_len == 0);
    EXPECT(tessera_theme_import(app, S(scheme), id, sizeof id, &id_len, msg, sizeof msg, &msg_len),
           TESSERA_ERROR_CONFLICT);
    CHECK(msg_len > 0);
    EXPECT(tessera_theme_import(app, S("/nonexistent/x.yaml"), id, sizeof id, &id_len, NULL, 0,
                                NULL),
           TESSERA_ERROR_IO);
    EXPECT(tessera_theme_import(app, S("/etc/hosts"), id, sizeof id, &id_len, NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    json = json_of(tessera_theme_list, app);
    CHECK(json != NULL);
    CHECK(strstr(json, "{\"id\":\"ocean\",\"name\":\"Ocean\",\"source\":\"user\"}") != NULL);
    free(json);
    /* The imported theme can be selected by its id. */
    EXPECT(tessera_config_set(app, S("appearance.theme"), S("\"ocean\""), NULL, 0, NULL),
           TESSERA_ERROR_OK);

    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
