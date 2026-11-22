/*
 * Command-finished notification smoke test of TesseraCore.h from C (#470,
 * ADR 0048), built and run by scripts/ffi-c-test.sh: the callback's
 * signature, the header's decision values and caps, and
 * tessera_app_command_finished_decision against a configuration with
 * command_finished = "unfocused" and command_finished_after = 2: nothing
 * for a NULL app, a short or a focused finish, ten posts, then marks only.
 * Uses a configuration directory of its own under the script's
 * TESSERA_CONFIG_DIR, so later tests start from no config.toml.
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "TesseraCore.h"

#define CHECK(cond)                                                                  \
    do {                                                                             \
        if (!(cond)) {                                                               \
            fprintf(stderr, "%s:%d: %s does not hold\n", __FILE__, __LINE__, #cond); \
            return 1;                                                                \
        }                                                                            \
    } while (0)

static const char CONFIG[] = "[notifications]\n"
                             "command_finished = \"unfocused\"\n"
                             "command_finished_after = 2\n";

/* The callback's C signature: assigning it to the field checks it. */
static void on_finished(void *user_data, uint64_t session_id, bool exit_known, int32_t exit_code,
                        uint64_t duration_ms) {
    (void)user_data;
    (void)session_id;
    (void)exit_known;
    (void)exit_code;
    (void)duration_ms;
}

int main(void) {
    CHECK(TESSERA_COMMAND_FINISHED_NOTHING == 0);
    CHECK(TESSERA_COMMAND_FINISHED_MARK == 1);
    CHECK(TESSERA_COMMAND_FINISHED_MARK_AND_POST == 2);
    CHECK(TESSERA_MAX_NOTIFICATION_TEXT_BYTES == 256);
    CHECK(TESSERA_MAX_NOTIFICATIONS_PER_MINUTE == 10);

    CHECK(tessera_app_command_finished_decision(NULL, 60000, false) ==
          TESSERA_COMMAND_FINISHED_NOTHING);

    const char *base = getenv("TESSERA_CONFIG_DIR");
    CHECK(base != NULL && base[0] == '/');
    char dir[1024];
    snprintf(dir, sizeof dir, "%s/notify-c-test", base);
    CHECK(mkdir(dir, 0700) == 0);
    CHECK(setenv("TESSERA_CONFIG_DIR", dir, 1) == 0);
    char config_path[1100];
    snprintf(config_path, sizeof config_path, "%s/config.toml", dir);
    FILE *file = fopen(config_path, "wb");
    CHECK(file != NULL);
    CHECK(fwrite(CONFIG, 1, strlen(CONFIG), file) == strlen(CONFIG));
    CHECK(fclose(file) == 0);

    TesseraApp *app = NULL;
    CHECK(tessera_app_new(&app) == TESSERA_ERROR_OK);
    TesseraCallbacks callbacks;
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.on_session_command_finished = on_finished;
    CHECK(tessera_app_set_callbacks(app, &callbacks) == TESSERA_ERROR_OK);

    CHECK(tessera_app_command_finished_decision(app, 1999, false) ==
          TESSERA_COMMAND_FINISHED_NOTHING);
    CHECK(tessera_app_command_finished_decision(app, 60000, true) ==
          TESSERA_COMMAND_FINISHED_NOTHING);
    for (int i = 0; i < TESSERA_MAX_NOTIFICATIONS_PER_MINUTE; i++) {
        CHECK(tessera_app_command_finished_decision(app, 2000, false) ==
              TESSERA_COMMAND_FINISHED_MARK_AND_POST);
    }
    CHECK(tessera_app_command_finished_decision(app, UINT64_MAX, false) ==
          TESSERA_COMMAND_FINISHED_MARK);

    tessera_app_free(app);
    CHECK(setenv("TESSERA_CONFIG_DIR", base, 1) == 0);
    puts("notify: ok");
    return 0;
}
