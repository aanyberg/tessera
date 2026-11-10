/*
 * Prompt navigation smoke test of TesseraCore.h from C (ADR 0039), built
 * and run by scripts/ffi-c-test.sh: spawn /bin/sh printing two commands
 * with their OSC 133 marks around 30 plain lines on a 24-row screen, wait
 * (at most 5 s) until Select Last Output selects the last command's
 * output, then walk the prompts with tessera_session_scroll_to_prompt and
 * ask tessera_session_has_command_output and the exit-status marks'
 * tessera_session_command_at, including NULL pointers and an unknown
 * session id.
 */

#include <stdio.h>
#include <string.h>
#include <time.h>

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

#define EXPECT_TRUE(cond)                                                            \
    do {                                                                             \
        if (!(cond)) {                                                               \
            fprintf(stderr, "%s:%d: expected %s\n", __FILE__, __LINE__, #cond);      \
            return 1;                                                                \
        }                                                                            \
    } while (0)

/* Moves session `id` to its previous (`prev`) or next prompt and checks
 * whether it moved and the offset it ends at. */
#define EXPECT_STEP(app, id, prev, want_moved, want_offset)                          \
    do {                                                                             \
        bool step_moved = !(want_moved);                                             \
        EXPECT(tessera_session_scroll_to_prompt((app), (id), (prev), &step_moved),   \
               TESSERA_ERROR_OK);                                                    \
        TesseraScrollPosition step_position;                                         \
        EXPECT(tessera_session_scroll_position((app), (id), &step_position),         \
               TESSERA_ERROR_OK);                                                    \
        if (step_moved != (want_moved) ||                                            \
            step_position.offset != (uint64_t)(want_offset)) {                       \
            fprintf(stderr, "%s:%d: moved %d to %llu, expected %d to %llu\n",        \
                    __FILE__, __LINE__, (int)step_moved,                             \
                    (unsigned long long)step_position.offset, (int)(want_moved),     \
                    (unsigned long long)(want_offset));                              \
            return 1;                                                                \
        }                                                                            \
    } while (0)

int main(void) {
    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    /* Line 0 is the first prompt, 1 and 2 its output, 3 to 32 plain
     * lines, 33 the second prompt and 34 its output ("hi"), 35 the live
     * prompt: 12 lines above the 24-row screen. */
    const char *script =
        "printf '\\033]133;A\\007$ \\033]133;B\\007ls\\n\\033]133;C\\007out1\\nout2\\n"
        "\\033]133;D;0\\007'; seq 1 30; "
        "printf '\\033]133;A\\007$ \\033]133;B\\007echo\\n\\033]133;C\\007hi\\n"
        "\\033]133;D;0\\007\\033]133;A\\007$ '; exec cat";
    const char *args[] = {"-c", script};
    TesseraSpawnOptions options = {
        .program = "/bin/sh",
        .args = args,
        .arg_count = 2,
        .cwd = NULL,
        .rows = 24,
        .cols = 80,
        .width_px = 0,
        .height_px = 0,
    };
    uint64_t id = 0;
    EXPECT(tessera_session_spawn(app, &options, &id), TESSERA_ERROR_OK);

    bool selected = false;
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        EXPECT(tessera_session_select_last_output(app, id, &selected), TESSERA_ERROR_OK);
        TesseraScrollPosition position;
        EXPECT(tessera_session_scroll_position(app, id, &position), TESSERA_ERROR_OK);
        if (selected && position.history == 12) {
            break;
        }
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec - start.tv_sec > 5) {
            fprintf(stderr, "timed out waiting for the output (history %llu)\n",
                    (unsigned long long)position.history);
            return 1;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }

    char text[64];
    size_t len = 0;
    EXPECT(tessera_session_selection_text(app, id, (uint8_t *)text, sizeof text, &len),
           TESSERA_ERROR_OK);
    if (len != 2 || memcmp(text, "hi", 2) != 0) {
        fprintf(stderr, "selected \"%.*s\", expected \"hi\"\n", (int)len, text);
        return 1;
    }

    bool has = false;
    EXPECT(tessera_session_has_command_output(app, id, &has), TESSERA_ERROR_OK);
    EXPECT_TRUE(has);

    /* Previous: the first prompt (line 0) at the top; then it stops. */
    EXPECT_STEP(app, id, true, true, 12);
    EXPECT_STEP(app, id, true, false, 12);
    /* Next: the second prompt is on the live screen. */
    EXPECT_STEP(app, id, false, true, 0);
    EXPECT_STEP(app, id, false, false, 0);

    /* The exit-status marks' hover (#45b): on the live screen row 21 is
     * the second prompt (line 33), whose command exited 0; row 22 ("hi")
     * and row 23 (the live prompt) have no mark. Scrolled to the top, row
     * 0 is the first prompt. Whether a duration is known depends on how
     * the output was chunked; when not, it is 0. */
    TesseraCommandMark mark = {.found = false, .exit_code = -1, .duration_ms = 7};
    EXPECT(tessera_session_command_at(app, id, 21, &mark), TESSERA_ERROR_OK);
    EXPECT_TRUE(mark.found && mark.exit_known && mark.exit_code == 0);
    EXPECT_TRUE(mark.duration_known || mark.duration_ms == 0);
    for (uint16_t row = 22; row <= 24; row++) {
        mark = (TesseraCommandMark){.found = true, .exit_known = true, .exit_code = 5};
        EXPECT(tessera_session_command_at(app, id, row, &mark), TESSERA_ERROR_OK);
        EXPECT_TRUE(!mark.found && !mark.exit_known && mark.exit_code == 0);
        EXPECT_TRUE(!mark.duration_known && mark.duration_ms == 0);
    }
    EXPECT_STEP(app, id, true, true, 12);
    mark = (TesseraCommandMark){.found = false};
    EXPECT(tessera_session_command_at(app, id, 0, &mark), TESSERA_ERROR_OK);
    EXPECT_TRUE(mark.found && mark.exit_known && mark.exit_code == 0);
    mark = (TesseraCommandMark){.found = true};
    EXPECT(tessera_session_command_at(NULL, id, 0, &mark), TESSERA_ERROR_NULL_POINTER);
    EXPECT_TRUE(!mark.found);
    EXPECT(tessera_session_command_at(app, id, 0, NULL), TESSERA_ERROR_NULL_POINTER);
    mark = (TesseraCommandMark){.found = true};
    EXPECT(tessera_session_command_at(app, id + 100, 0, &mark),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT_TRUE(!mark.found);

    /* NULL pointers and an unknown session id; the out value is false. */
    bool out = true;
    EXPECT(tessera_session_scroll_to_prompt(NULL, id, true, &out), TESSERA_ERROR_NULL_POINTER);
    EXPECT_TRUE(!out);
    EXPECT(tessera_session_scroll_to_prompt(app, id, true, NULL), TESSERA_ERROR_NULL_POINTER);
    out = true;
    EXPECT(tessera_session_select_last_output(NULL, id, &out), TESSERA_ERROR_NULL_POINTER);
    EXPECT_TRUE(!out);
    EXPECT(tessera_session_select_last_output(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    out = true;
    EXPECT(tessera_session_has_command_output(NULL, id, &out), TESSERA_ERROR_NULL_POINTER);
    EXPECT_TRUE(!out);
    EXPECT(tessera_session_has_command_output(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    out = true;
    EXPECT(tessera_session_scroll_to_prompt(app, id + 100, false, &out),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT_TRUE(!out);
    out = true;
    EXPECT(tessera_session_select_last_output(app, id + 100, &out),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT_TRUE(!out);
    out = true;
    EXPECT(tessera_session_has_command_output(app, id + 100, &out),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT_TRUE(!out);

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    tessera_app_free(app);
    printf("prompts: ok\n");
    return 0;
}
