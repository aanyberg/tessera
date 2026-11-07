/*
 * Key binding smoke test of TesseraCore.h from C (#43), built and run by
 * scripts/ffi-c-test.sh: read the key map JSON with the two-call protocol,
 * check that Command-T reports a command without being typed (with and
 * without TESSERA_HANDLE_KEY_BINDINGS_ONLY), that an unbound key under the
 * flag is ignored, and that unknown flag bits are refused. The callback
 * table carries on_config_applied, appended last.
 */

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "TesseraCore.h"

/* Appended after on_session_clipboard (#43): the table's last field. */
_Static_assert(offsetof(TesseraCallbacks, on_config_applied) ==
                   offsetof(TesseraCallbacks, on_session_clipboard) + sizeof(void (*)(void)),
               "on_config_applied follows on_session_clipboard");
_Static_assert(sizeof(TesseraCallbacks) ==
                   offsetof(TesseraCallbacks, on_config_applied) + sizeof(void (*)(void)),
               "on_config_applied is the last field");

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
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
