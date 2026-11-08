/*
 * Key binding smoke test of TesseraCore.h from C (#43), built and run by
 * scripts/ffi-c-test.sh: read the key map JSON with the two-call protocol,
 * check that Command-T reports a command without being typed (with and
 * without TESSERA_HANDLE_KEY_BINDINGS_ONLY), that an unbound key under the
 * flag is ignored, and that unknown flag bits are refused. The callback
 * table carries on_config_applied, appended after on_session_clipboard
 * (remote.c checks the table's last field, on_session_remote, #205). #317:
 * a chord with an invisible key is refused with an escaped message, and
 * once a binding applies the JSON carries the new generation with the new
 * chords. Uses a configuration directory of its own under the script's
 * TESSERA_CONFIG_DIR, so later tests start from no config.toml.
 */

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>

#include "TesseraCore.h"

/* Appended after on_session_clipboard (#43). */
_Static_assert(offsetof(TesseraCallbacks, on_config_applied) ==
                   offsetof(TesseraCallbacks, on_session_clipboard) + sizeof(void (*)(void)),
               "on_config_applied follows on_session_clipboard");

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

static void on_config_applied_cb(void *user_data, uint64_t generation) {
    (void)user_data;
    (void)generation;
}

#define S(text) (const uint8_t *)(text), strlen(text)

/* The key map JSON, NUL-terminated, read with the two-call protocol (with
 * room to spare: the map may grow between the calls); NULL on failure. */
static char *keymap_json(const TesseraApp *app) {
    size_t len = 0;
    if (tessera_app_keymap_json(app, NULL, 0, &len) != TESSERA_ERROR_INVALID_ARGUMENT ||
        len == 0) {
        return NULL;
    }
    size_t cap = len + 4096;
    char *json = malloc(cap + 1);
    size_t written = 0;
    if (json == NULL ||
        tessera_app_keymap_json(app, (uint8_t *)json, cap, &written) != TESSERA_ERROR_OK) {
        free(json);
        return NULL;
    }
    json[written] = 0;
    return json;
}

static TesseraKeyEvent key(uint32_t key, uint32_t codepoint, uint8_t modifiers) {
    TesseraKeyEvent event;
    memset(&event, 0, sizeof event);
    event.key = key;
    event.codepoint = codepoint;
    event.modifiers = modifiers;
    event.kind = TESSERA_KEY_EVENT_KIND_PRESS;
    event.text = NULL;
    event.text_len = 0;
    return event;
}

int main(void) {
    const char *base = getenv("TESSERA_CONFIG_DIR");
    CHECK(base != NULL && base[0] == '/');
    char dir[1024];
    snprintf(dir, sizeof dir, "%s/keymap-c-test", base);
    CHECK(mkdir(dir, 0700) == 0);
    CHECK(setenv("TESSERA_CONFIG_DIR", dir, 1) == 0);

    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);
    TesseraCallbacks callbacks = {
        .user_data = NULL,
        .on_session_output = NULL,
        .on_session_title = NULL,
        .on_session_bell = NULL,
        .on_session_exit = NULL,
        .on_appearance_changed = NULL,
        .on_session_clipboard = NULL,
        /* Appended last (#43). */
        .on_config_applied = on_config_applied_cb,
    };
    EXPECT(tessera_app_set_callbacks(app, &callbacks), TESSERA_ERROR_OK);

    /* The key map: ask for the size, then read it. */
    size_t len = 0;
    EXPECT(tessera_app_keymap_json(app, NULL, 0, &len), TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(len > 0);
    uint8_t *json = malloc(len + 1);
    CHECK(json != NULL);
    size_t written = 0;
    EXPECT(tessera_app_keymap_json(app, json, len, &written), TESSERA_ERROR_OK);
    CHECK(written == len);
    json[len] = 0;
    CHECK(strstr((const char *)json, "\"command\":\"new_tab\"") != NULL);
    CHECK(strstr((const char *)json, "\"generation\":0") != NULL);
    free(json);

    TesseraSpawnOptions options = {
        .program = "/bin/cat",
        .args = NULL,
        .arg_count = 0,
        .cwd = NULL,
        .rows = 24,
        .cols = 80,
        .width_px = 0,
        .height_px = 0,
    };
    uint64_t id = 0;
    EXPECT(tessera_session_spawn(app, &options, &id), TESSERA_ERROR_OK);

    TesseraInputOutcome outcome = TESSERA_INPUT_OUTCOME_IGNORED;
    uint32_t command = TESSERA_NO_COMMAND;
    TesseraKeyEvent cmd_t = key(TESSERA_KEY_CHAR, 't', TESSERA_MODIFIERS_SUPER);
    EXPECT(tessera_session_handle_key(app, id, &cmd_t, TESSERA_HANDLE_KEY_BINDINGS_ONLY, &outcome,
                                      &command),
           TESSERA_ERROR_OK);
    CHECK(outcome == TESSERA_INPUT_OUTCOME_COMMAND);
    CHECK(command != TESSERA_NO_COMMAND);
    uint32_t new_tab = command;
    EXPECT(tessera_session_handle_key(app, id, &cmd_t, 0, &outcome, &command), TESSERA_ERROR_OK);
    CHECK(outcome == TESSERA_INPUT_OUTCOME_COMMAND && command == new_tab);

    TesseraKeyEvent x = key(TESSERA_KEY_CHAR, 'x', TESSERA_MODIFIERS_NONE);
    EXPECT(tessera_session_handle_key(app, id, &x, TESSERA_HANDLE_KEY_BINDINGS_ONLY, &outcome,
                                      &command),
           TESSERA_ERROR_OK);
    CHECK(outcome == TESSERA_INPUT_OUTCOME_IGNORED && command == TESSERA_NO_COMMAND);
    EXPECT(tessera_session_handle_key(app, id, &x, 0, &outcome, &command), TESSERA_ERROR_OK);
    CHECK(outcome == TESSERA_INPUT_OUTCOME_CONSUMED && command == TESSERA_NO_COMMAND);

    EXPECT(tessera_session_handle_key(app, id, &x, 2, &outcome, &command),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(outcome == TESSERA_INPUT_OUTCOME_IGNORED && command == TESSERA_NO_COMMAND);

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);

    /* #317: a chord whose key is a zero-width space (U+200B) is refused,
     * and the message shows the key escaped, never the raw character. */
    uint8_t msg[512];
    size_t msg_len = 0;
    EXPECT(tessera_config_set(app, S("keys.\"cmd+\xe2\x80\x8b\""), S("\"new_tab\""), msg,
                              sizeof msg - 1, &msg_len),
           TESSERA_ERROR_REFUSED);
    CHECK(msg_len > 0 && msg_len < sizeof msg);
    msg[msg_len] = 0;
    /* The path before "refused:" is the caller's own key, echoed back; the
     * reason after it is the key map's warning, escaped. */
    const char *reason = strstr((const char *)msg, "refused:");
    CHECK(reason != NULL);
    CHECK(strstr(reason, "\xe2\x80\x8b") == NULL);
    CHECK(strstr(reason, "invisible key `\\u{200b}`") != NULL);

    /* A binding applies: the JSON that shows the new chords carries a
     * later generation, read with them. */
    EXPECT(tessera_config_set(app, S("keys.\"cmd+u\""), S("\"new_tab\""), NULL, 0, NULL),
           TESSERA_ERROR_OK);
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        char *text = keymap_json(app);
        CHECK(text != NULL);
        int initial = strstr(text, "\"generation\":0") != NULL;
        int rebound = strstr(text, "\"chords\":[\"cmd+u\",\"cmd+t\"]") != NULL;
        free(text);
        /* The watch may apply the created default file first; the chords
         * of the edit never come with the generation the app started at. */
        CHECK(!(initial && rebound));
        if (rebound) {
            break;
        }
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec - start.tv_sec > 30) {
            fprintf(stderr, "timed out waiting for the configuration to apply\n");
            return 1;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
