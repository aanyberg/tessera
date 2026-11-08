/*
 * Accessibility smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh printing two lines, wait (at most
 * 5 s) until a snapshot of the view reads them, look its lines, characters
 * and cells up, then type a line into cat and read the announcement for
 * the output, into a short and a long buffer, including NULL pointers and
 * an unknown session id.
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

#define CHECK(cond)                                                                  \
    do {                                                                             \
        if (!(cond)) {                                                               \
            fprintf(stderr, "%s:%d: %s failed\n", __FILE__, __LINE__, #cond);        \
            return 1;                                                                \
        }                                                                            \
    } while (0)

/* Whether the snapshot's text starts with the ASCII string `want`. */
static int starts_with(const TesseraAccessibleTextContents *contents, const char *want) {
    size_t n = strlen(want);
    if (contents->length < n) {
        return 0;
    }
    for (size_t i = 0; i < n; i++) {
        if (contents->text[i] != (uint16_t)(unsigned char)want[i]) {
            return 0;
        }
    }
    return 1;
}

/* Takes snapshots of session `id` until one starts with `want` (5 s at most). */
static TesseraAccessibleText *wait_for(TesseraApp *app, uint64_t id, const char *want) {
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        TesseraAccessibleText *text = NULL;
        if (tessera_session_accessible_text(app, id, &text) != TESSERA_ERROR_OK || text == NULL) {
            fprintf(stderr, "no snapshot\n");
            return NULL;
        }
        TesseraAccessibleTextContents contents;
        if (tessera_accessible_text_contents(text, &contents) == TESSERA_ERROR_OK &&
            starts_with(&contents, want)) {
            return text;
        }
        tessera_accessible_text_free(text);
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec - start.tv_sec > 5) {
            fprintf(stderr, "timed out waiting for \"%s\"\n", want);
            return NULL;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
}

int main(void) {
    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    const char *args[] = {"-c", "stty -echo; printf 'hello\\nworld\\n'; exec cat"};
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

    TesseraAccessibleText *before = wait_for(app, id, "hello\nworld\n");
    CHECK(before != NULL);
    TesseraAccessibleTextContents contents;
    EXPECT(tessera_accessible_text_contents(before, &contents), TESSERA_ERROR_OK);
    /* Two lines and the cursor's empty one: "hello\nworld\n" (D-3). */
    CHECK(contents.length == 12);
    CHECK(contents.line_count == 3);
    CHECK(contents.has_cursor);
    CHECK(contents.insertion_point == 12);
    CHECK(!contents.has_selection);
    CHECK(contents.selected.location == 12 && contents.selected.length == 0);

    size_t line = 99;
    EXPECT(tessera_accessible_text_line_for_index(before, 7, &line), TESSERA_ERROR_OK);
    CHECK(line == 1);
    TesseraTextRange range = {0, 0};
    EXPECT(tessera_accessible_text_line_range(before, 1, &range), TESSERA_ERROR_OK);
    CHECK(range.location == 6 && range.length == 6);
    EXPECT(tessera_accessible_text_line_range(before, 3, &range), TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(range.location == 0 && range.length == 0);
    EXPECT(tessera_accessible_text_char_range(before, 5, &range), TESSERA_ERROR_OK);
    CHECK(range.location == 5 && range.length == 1);
    EXPECT(tessera_accessible_text_range_at_cell(before, 2, 1, &range), TESSERA_ERROR_OK);
    CHECK(range.location == 8 && range.length == 1);
    TesseraCellRect rect = {9, 9, 9, 9};
    TesseraTextRange world = {6, 5};
    EXPECT(tessera_accessible_text_cells_for_range(before, world, &rect), TESSERA_ERROR_OK);
    CHECK(rect.row == 1 && rect.col == 0 && rect.rows == 1 && rect.cols == 5);

    /* Nothing changed: nothing to announce. */
    size_t len = 99;
    EXPECT(tessera_accessible_text_announcement(before, before, NULL, 0, &len), TESSERA_ERROR_OK);
    CHECK(len == 0);

    const char *input = "typed\n";
    EXPECT(tessera_session_write(app, id, (const uint8_t *)input, strlen(input)), TESSERA_ERROR_OK);
    TesseraAccessibleText *after = wait_for(app, id, "hello\nworld\ntyped\n");
    CHECK(after != NULL);

    /* A size query, a short buffer, a NULL buffer with room, then the text. */
    uint16_t buf[64];
    len = 0;
    EXPECT(tessera_accessible_text_announcement(before, after, NULL, 0, &len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(len == 5);
    memset(buf, 0xff, sizeof buf);
    EXPECT(tessera_accessible_text_announcement(before, after, buf, 2, &len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(len == 5 && buf[0] == 0xffff);
    len = 99;
    EXPECT(tessera_accessible_text_announcement(before, after, NULL, sizeof buf / sizeof buf[0], &len),
           TESSERA_ERROR_NULL_POINTER);
    CHECK(len == 0);
    EXPECT(tessera_accessible_text_announcement(before, after, buf, sizeof buf / sizeof buf[0], &len),
           TESSERA_ERROR_OK);
    CHECK(len == 5);
    for (size_t i = 0; i < 5; i++) {
        CHECK(buf[i] == (uint16_t)"typed"[i]);
    }
    CHECK(buf[5] == 0xffff);

    /* One unit short, then an exact fit (cap == len). */
    uint16_t exact[5];
    memset(exact, 0xff, sizeof exact);
    EXPECT(tessera_accessible_text_announcement(before, after, exact, 4, &len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(len == 5 && exact[0] == 0xffff);
    len = 0;
    EXPECT(tessera_accessible_text_announcement(before, after, exact, 5, &len), TESSERA_ERROR_OK);
    CHECK(len == 5);
    for (size_t i = 0; i < 5; i++) {
        CHECK(exact[i] == (uint16_t)"typed"[i]);
    }

    /* NULL pointers and an unknown session. */
    TesseraAccessibleText *none = (TesseraAccessibleText *)&contents;
    EXPECT(tessera_session_accessible_text(NULL, id, &none), TESSERA_ERROR_NULL_POINTER);
    CHECK(none == NULL);
    EXPECT(tessera_session_accessible_text(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    none = (TesseraAccessibleText *)&contents;
    EXPECT(tessera_session_accessible_text(app, id + 1000, &none), TESSERA_ERROR_SESSION_NOT_FOUND);
    CHECK(none == NULL);
    EXPECT(tessera_accessible_text_contents(NULL, &contents), TESSERA_ERROR_NULL_POINTER);
    CHECK(contents.length == 0 && contents.text != NULL);
    EXPECT(tessera_accessible_text_contents(after, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_accessible_text_line_for_index(NULL, 0, &line), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_accessible_text_line_range(after, 0, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_accessible_text_announcement(NULL, after, buf, 64, &len), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_accessible_text_announcement(before, after, buf, 64, NULL), TESSERA_ERROR_NULL_POINTER);
    tessera_accessible_text_free(NULL);

    /* A snapshot outlives its session. */
    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    EXPECT(tessera_accessible_text_contents(after, &contents), TESSERA_ERROR_OK);
    CHECK(starts_with(&contents, "hello\nworld\ntyped\n"));

    tessera_accessible_text_free(before);
    tessera_accessible_text_free(after);
    tessera_app_free(app);
    puts("accessibility: ok");
    return 0;
}
