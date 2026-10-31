/*
 * Selection smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh printing two lines, wait (at most
 * 5 s) until Select All copies both, then select with a click and drag,
 * adjust with a shift-click, copy into a short and a long buffer, select a
 * word, hit-test a URL and set a hover, including NULL pointers, an
 * unknown granularity and an unknown session id.
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

/* Copies session `id`'s selection into `buf` and compares it with `want`. */
#define EXPECT_TEXT(app, id, want)                                                   \
    do {                                                                             \
        char text_buf[256];                                                          \
        size_t text_len = 99;                                                        \
        EXPECT(tessera_session_selection_text((app), (id), (uint8_t *)text_buf,      \
                                              sizeof text_buf, &text_len),           \
               TESSERA_ERROR_OK);                                                    \
        if (text_len != strlen(want) || memcmp(text_buf, (want), text_len) != 0) {   \
            fprintf(stderr, "%s:%d: selection \"%.*s\", expected \"%s\"\n", __FILE__,  \
                    __LINE__, (int)text_len, text_buf, (want));                      \
            return 1;                                                                \
        }                                                                            \
    } while (0)

int main(void) {
    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    const char *args[] = {"-c", "printf 'hello world\\nsee https://a.bc/x. ok\\n'; exec cat"};
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

    const char *all = "hello world\nsee https://a.bc/x. ok";
    char buf[256];
    size_t len = 0;
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        EXPECT(tessera_session_select_all(app, id), TESSERA_ERROR_OK);
        EXPECT(tessera_session_selection_text(app, id, (uint8_t *)buf, sizeof buf, &len),
               TESSERA_ERROR_OK);
        if (len == strlen(all) && memcmp(buf, all, len) == 0) {
            break;
        }
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec - start.tv_sec > 5) {
            fprintf(stderr, "timed out waiting for the output (\"%.*s\")\n", (int)len, buf);
            return 1;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }

    /* A click selects nothing; a drag selects whole cells. */
    bool has = true;
    EXPECT(tessera_session_select_begin(app, id, 0, 0, TESSERA_GRANULARITY_CHAR, false),
           TESSERA_ERROR_OK);
    EXPECT(tessera_session_has_selection(app, id, &has), TESSERA_ERROR_OK);
    if (has) {
        fprintf(stderr, "a click alone selected something\n");
        return 1;
    }
    len = 99;
    EXPECT(tessera_session_selection_text(app, id, (uint8_t *)buf, sizeof buf, &len),
           TESSERA_ERROR_OK);
    if (len != 0) {
        fprintf(stderr, "no selection gave %zu bytes\n", len);
        return 1;
    }
    EXPECT(tessera_session_select_extend(app, id, 4, 0), TESSERA_ERROR_OK);
    EXPECT(tessera_session_has_selection(app, id, &has), TESSERA_ERROR_OK);
    if (!has) {
        fprintf(stderr, "a drag selected nothing\n");
        return 1;
    }
    EXPECT_TEXT(app, id, "hello");

    /* Shift-click past the end moves the end. */
    EXPECT(tessera_session_select_adjust(app, id, 10, 0), TESSERA_ERROR_OK);
    EXPECT_TEXT(app, id, "hello world");

    /* A short buffer: the size needed, nothing written past the cap. */
    memset(buf, '#', sizeof buf);
    len = 0;
    EXPECT(tessera_session_selection_text(app, id, (uint8_t *)buf, 3, &len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    if (len != 11 || buf[0] != '#' || buf[3] != '#') {
        fprintf(stderr, "short buffer: len %zu, buf \"%.4s\"\n", len, buf);
        return 1;
    }
    len = 0;
    EXPECT(tessera_session_selection_text(app, id, NULL, 0, &len), TESSERA_ERROR_INVALID_ARGUMENT);
    if (len != 11) {
        fprintf(stderr, "size query gave %zu\n", len);
        return 1;
    }

    /* A NULL buffer with room claimed: nothing written, *len reset to 0. */
    len = 99;
    EXPECT(tessera_session_selection_text(app, id, NULL, sizeof buf, &len),
           TESSERA_ERROR_NULL_POINTER);
    if (len != 0) {
        fprintf(stderr, "NULL buffer left len %zu\n", len);
        return 1;
    }

    /* A double-click selects a word; clearing leaves no selection. */
    EXPECT(tessera_session_select_begin(app, id, 7, 0, TESSERA_GRANULARITY_WORD, false),
           TESSERA_ERROR_OK);
    EXPECT_TEXT(app, id, "world");
    EXPECT(tessera_session_select_clear(app, id), TESSERA_ERROR_OK);
    EXPECT(tessera_session_has_selection(app, id, &has), TESSERA_ERROR_OK);
    if (has) {
        fprintf(stderr, "cleared selection still there\n");
        return 1;
    }

    /* The URL under a cell, without the trailing period; none beside it. */
    len = 0;
    EXPECT(tessera_session_link_at(app, id, 10, 1, (uint8_t *)buf, sizeof buf, &len),
           TESSERA_ERROR_OK);
    if (len != strlen("https://a.bc/x") || memcmp(buf, "https://a.bc/x", len) != 0) {
        fprintf(stderr, "link \"%.*s\"\n", (int)len, buf);
        return 1;
    }
    len = 99;
    EXPECT(tessera_session_link_at(app, id, 18, 1, (uint8_t *)buf, sizeof buf, &len),
           TESSERA_ERROR_OK);
    if (len != 0) {
        fprintf(stderr, "a link on the trailing period (%zu bytes)\n", len);
        return 1;
    }
    EXPECT(tessera_session_link_at(app, id, 10, 1, NULL, 0, &len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT(tessera_session_set_hover(app, id, 10, 1, true), TESSERA_ERROR_OK);
    EXPECT(tessera_session_set_hover(app, id, 0, 0, false), TESSERA_ERROR_OK);

    /* Bad arguments. */
    EXPECT(tessera_session_select_begin(app, id, 0, 0, 7, false), TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT(tessera_session_select_begin(NULL, id, 0, 0, 7, false), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_select_extend(NULL, id, 0, 0), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_has_selection(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_selection_text(app, id, (uint8_t *)buf, sizeof buf, NULL),
           TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_link_at(NULL, id, 0, 0, (uint8_t *)buf, sizeof buf, &len),
           TESSERA_ERROR_NULL_POINTER);
    uint64_t unknown = id + 1000;
    has = true;
    EXPECT(tessera_session_has_selection(app, unknown, &has), TESSERA_ERROR_SESSION_NOT_FOUND);
    if (has) {
        fprintf(stderr, "has_selection not false after an unknown session\n");
        return 1;
    }
    EXPECT(tessera_session_select_all(app, unknown), TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT(tessera_session_select_adjust(app, unknown, 0, 0), TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT(tessera_session_select_clear(app, unknown), TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT(tessera_session_set_hover(app, unknown, 0, 0, true), TESSERA_ERROR_SESSION_NOT_FOUND);
    len = 5;
    EXPECT(tessera_session_selection_text(app, unknown, (uint8_t *)buf, sizeof buf, &len),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    if (len != 0) {
        fprintf(stderr, "len not zeroed after an unknown session\n");
        return 1;
    }

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
