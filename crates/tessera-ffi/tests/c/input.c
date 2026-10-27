/*
 * Input smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh, paste "exit " and type "3" and
 * Return with tessera_session_send_key, then wait (at most 5 s) for the
 * shell to exit with code 3. Along the way, check the outcome enum of keys
 * that send nothing, of mouse and scroll events, and of malformed events.
 */

#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "TesseraCore.h"

struct recorder {
    pthread_mutex_t lock;
    pthread_cond_t changed;
    int exited;
    int32_t code;
};

static void on_exit_cb(void *user_data, uint64_t session_id, int32_t code, int32_t signal) {
    struct recorder *r = user_data;
    (void)session_id;
    (void)signal;
    pthread_mutex_lock(&r->lock);
    r->exited = 1;
    r->code = code;
    pthread_cond_broadcast(&r->changed);
    pthread_mutex_unlock(&r->lock);
}

#define EXPECT(expr, status)                                                         \
    do {                                                                             \
        TesseraError expect_status = (expr);                                         \
        if (expect_status != (status)) {                                             \
            fprintf(stderr, "%s:%d: %s returned %d, expected %d\n", __FILE__, __LINE__, \
                    #expr, (int)expect_status, (int)(status));                       \
            return 1;                                                                \
        }                                                                            \
    } while (0)

#define EXPECT_OUTCOME(outcome, expected)                                            \
    do {                                                                             \
        if ((outcome) != (expected)) {                                               \
            fprintf(stderr, "%s:%d: outcome %d, expected %d\n", __FILE__, __LINE__,  \
                    (int)(outcome), (int)(expected));                                \
            return 1;                                                                \
        }                                                                            \
    } while (0)

static TesseraKeyEvent key(uint32_t key, uint32_t codepoint) {
    TesseraKeyEvent event;
    memset(&event, 0, sizeof event);
    event.key = key;
    event.codepoint = codepoint;
    event.modifiers = TESSERA_MODIFIERS_NONE;
    event.kind = TESSERA_KEY_EVENT_KIND_PRESS;
    event.text = NULL;
    event.text_len = 0;
    return event;
}

int main(void) {
    struct recorder rec;
    memset(&rec, 0, sizeof rec);
    pthread_mutex_init(&rec.lock, NULL);
    pthread_cond_init(&rec.changed, NULL);

    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);
    TesseraCallbacks callbacks = {
        .user_data = &rec,
        .on_session_output = NULL,
        .on_session_title = NULL,
        .on_session_bell = NULL,
        .on_session_exit = on_exit_cb,
    };
    EXPECT(tessera_app_set_callbacks(app, &callbacks), TESSERA_ERROR_OK);

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
    EXPECT(tessera_session_spawn(app, &options, &id), TESSERA_ERROR_OK);
    EXPECT(tessera_session_set_focus(app, id, true), TESSERA_ERROR_OK);

    TesseraInputOutcome outcome = TESSERA_INPUT_OUTCOME_CONSUMED;

    /* Keys that send nothing are left to the frontend. */
    TesseraKeyEvent shift = key(TESSERA_KEY_LEFT_SHIFT, 0);
    shift.modifiers = TESSERA_MODIFIERS_SHIFT;
    EXPECT(tessera_session_send_key(app, id, &shift, &outcome), TESSERA_ERROR_OK);
    EXPECT_OUTCOME(outcome, TESSERA_INPUT_OUTCOME_IGNORED);

    /* A malformed event is rejected and reads as ignored. */
    TesseraKeyEvent bogus = key(0xdeadbeef, 0);
    outcome = TESSERA_INPUT_OUTCOME_CONSUMED;
    EXPECT(tessera_session_send_key(app, id, &bogus, &outcome), TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT_OUTCOME(outcome, TESSERA_INPUT_OUTCOME_IGNORED);

    /* Mouse events are recorded but left to the frontend; there is no
     * scrollback to scroll into yet. */
    TesseraMouseEvent mouse = {
        .kind = TESSERA_MOUSE_EVENT_KIND_PRESS,
        .button = TESSERA_MOUSE_BUTTON_LEFT,
        .col = 2,
        .row = 1,
        .x_px = 20.0f,
        .y_px = 18.0f,
        .modifiers = TESSERA_MODIFIERS_NONE,
    };
    EXPECT(tessera_session_send_mouse(app, id, &mouse, &outcome), TESSERA_ERROR_OK);
    EXPECT_OUTCOME(outcome, TESSERA_INPUT_OUTCOME_IGNORED);
    TesseraScrollEvent scroll = {
        .delta_lines = 3.0f,
        .delta_x_px = 0.0f,
        .delta_y_px = 0.0f,
        .precise = 0,
        .col = 0,
        .row = 0,
        .modifiers = TESSERA_MODIFIERS_NONE,
    };
    EXPECT(tessera_session_send_scroll(app, id, &scroll, &outcome), TESSERA_ERROR_OK);
    EXPECT_OUTCOME(outcome, TESSERA_INPUT_OUTCOME_IGNORED);

    /* "exit " by paste, then "3" and Return by key. */
    static const char paste[] = "exit ";
    EXPECT(tessera_session_paste(app, id, paste, strlen(paste)), TESSERA_ERROR_OK);
    TesseraKeyEvent three = key(TESSERA_KEY_CHAR, '3');
    EXPECT(tessera_session_send_key(app, id, &three, &outcome), TESSERA_ERROR_OK);
    EXPECT_OUTCOME(outcome, TESSERA_INPUT_OUTCOME_CONSUMED);
    TesseraKeyEvent enter = key(TESSERA_KEY_ENTER, 0);
    outcome = TESSERA_INPUT_OUTCOME_IGNORED;
    EXPECT(tessera_session_send_key(app, id, &enter, &outcome), TESSERA_ERROR_OK);
    EXPECT_OUTCOME(outcome, TESSERA_INPUT_OUTCOME_CONSUMED);

    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += 5;
    pthread_mutex_lock(&rec.lock);
    int wait_status = 0;
    while (!rec.exited && wait_status == 0) {
        wait_status = pthread_cond_timedwait(&rec.changed, &rec.lock, &deadline);
    }
    int exited = rec.exited;
    int32_t code = rec.code;
    pthread_mutex_unlock(&rec.lock);
    if (!exited) {
        fprintf(stderr, "timed out waiting for the shell to exit\n");
        return 1;
    }
    if (code != 3) {
        fprintf(stderr, "shell exited with %d, expected 3\n", code);
        return 1;
    }
    printf("shell exited with code %d after a pasted and typed command\n", code);

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    tessera_app_free(app);
    pthread_cond_destroy(&rec.changed);
    pthread_mutex_destroy(&rec.lock);
    printf("ok\n");
    return 0;
}
