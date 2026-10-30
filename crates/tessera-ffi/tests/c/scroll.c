/*
 * Scrollback smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh printing 101 lines on a 24-row
 * screen, wait (at most 5 s) for the 78 lines that leave the screen, then
 * move the view with tessera_session_scroll_to and read it back with
 * tessera_session_scroll_position, including a NULL out pointer, a NULL
 * app, an unknown target and an unknown session id.
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

#define EXPECT_POSITION(app, id, want_offset, want_history)                          \
    do {                                                                             \
        TesseraScrollPosition got;                                                   \
        EXPECT(tessera_session_scroll_position((app), (id), &got), TESSERA_ERROR_OK);  \
        if (got.offset != (uint64_t)(want_offset) ||                                 \
            got.history != (uint64_t)(want_history) || got.rows != 24) {             \
            fprintf(stderr, "%s:%d: position %llu/%llu/%u, expected %llu/%llu/24\n", \
                    __FILE__, __LINE__, (unsigned long long)got.offset,              \
                    (unsigned long long)got.history, (unsigned)got.rows,             \
                    (unsigned long long)(want_offset),                               \
                    (unsigned long long)(want_history));                             \
            return 1;                                                                \
        }                                                                            \
    } while (0)

int main(void) {
    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    /* 100 numbered lines and "ready" leave the cursor on the 102nd row:
     * 78 rows above the 24-row screen. `cat` then waits for input. */
    const char *args[] = {"-c", "seq 1 100; echo ready; exec cat"};
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

    TesseraScrollPosition position;
    memset(&position, 0, sizeof position);
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        EXPECT(tessera_session_scroll_position(app, id, &position), TESSERA_ERROR_OK);
        if (position.history >= 78) {
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
    EXPECT_POSITION(app, id, 0, 78);

    /* Every target, clamped to the history; a page is rows - 1 lines. */
    EXPECT(tessera_session_scroll_to(app, id, TESSERA_SCROLL_TARGET_TOP, 0), TESSERA_ERROR_OK);
    EXPECT_POSITION(app, id, 78, 78);
    EXPECT(tessera_session_scroll_to(app, id, TESSERA_SCROLL_TARGET_PAGE_DOWN, 0),
           TESSERA_ERROR_OK);
    EXPECT_POSITION(app, id, 55, 78);
    EXPECT(tessera_session_scroll_to(app, id, TESSERA_SCROLL_TARGET_PAGE_UP, 0), TESSERA_ERROR_OK);
    EXPECT_POSITION(app, id, 78, 78);
    EXPECT(tessera_session_scroll_to(app, id, TESSERA_SCROLL_TARGET_OFFSET, 10), TESSERA_ERROR_OK);
    EXPECT_POSITION(app, id, 10, 78);
    EXPECT(tessera_session_scroll_to(app, id, TESSERA_SCROLL_TARGET_OFFSET, UINT64_MAX),
           TESSERA_ERROR_OK);
    EXPECT_POSITION(app, id, 78, 78);
    EXPECT(tessera_session_scroll_to(app, id, TESSERA_SCROLL_TARGET_BOTTOM, 5), TESSERA_ERROR_OK);
    EXPECT_POSITION(app, id, 0, 78);

    /* An unknown target moves nothing. */
    EXPECT(tessera_session_scroll_to(app, id, TESSERA_SCROLL_TARGET_OFFSET, 20), TESSERA_ERROR_OK);
    EXPECT(tessera_session_scroll_to(app, id, 99, 0), TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT_POSITION(app, id, 20, 78);

    /* NULL pointers and an unknown session; *out is zeroed on failure. */
    EXPECT(tessera_session_scroll_position(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    position.offset = 7;
    EXPECT(tessera_session_scroll_position(NULL, id, &position), TESSERA_ERROR_NULL_POINTER);
    if (position.offset != 0 || position.history != 0 || position.rows != 0) {
        fprintf(stderr, "position not zeroed after a NULL app\n");
        return 1;
    }
    EXPECT(tessera_session_scroll_to(NULL, id, TESSERA_SCROLL_TARGET_TOP, 0),
           TESSERA_ERROR_NULL_POINTER);
    uint64_t unknown = id + 1000;
    position.history = 7;
    EXPECT(tessera_session_scroll_position(app, unknown, &position),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    if (position.offset != 0 || position.history != 0 || position.rows != 0) {
        fprintf(stderr, "position not zeroed after an unknown session\n");
        return 1;
    }
    EXPECT(tessera_session_scroll_to(app, unknown, TESSERA_SCROLL_TARGET_TOP, 0),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT_POSITION(app, id, 20, 78);

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    EXPECT(tessera_session_scroll_position(app, id, &position), TESSERA_ERROR_SESSION_NOT_FOUND);
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
