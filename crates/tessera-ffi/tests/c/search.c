/*
 * Search smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh printing three lines, step a literal
 * search (waiting at most 5 s for the output), move between the matches
 * both ways, reveal the current one without moving it, copy them all into
 * a short and a long buffer, then a
 * case-sensitive regex, refused patterns (invalid and too complex regex,
 * too long, not UTF-8, an unknown option), clearing, NULL pointers and an
 * unknown session id.
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

#define CHECK(cond, ...)                                                             \
    do {                                                                             \
        if (!(cond)) {                                                               \
            fprintf(stderr, "%s:%d: ", __FILE__, __LINE__);                          \
            fprintf(stderr, __VA_ARGS__);                                            \
            fprintf(stderr, "\n");                                                   \
            return 1;                                                                \
        }                                                                            \
    } while (0)

/* Starts a search for the NUL-terminated `pattern`. */
static TesseraError start(TesseraApp *app, uint64_t id, const char *pattern, uint32_t options,
                          uint32_t *refusal) {
    return tessera_session_search_start(app, id, (const uint8_t *)pattern, strlen(pattern),
                                        options, refusal, NULL, 0, NULL);
}

/* Steps the search until it is done. */
static TesseraError finish(TesseraApp *app, uint64_t id, TesseraSearchStatus *status) {
    for (int i = 0; i < 1000; i++) {
        TesseraError error = tessera_session_search_step(app, id, 2000, status);
        if (error != TESSERA_ERROR_OK || status->done) {
            return error;
        }
    }
    return TESSERA_ERROR_OK;
}

int main(void) {
    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    const char *args[] = {"-c", "printf 'needle 1\\nNEEDLE 2\\nhay\\nneedle 3\\n'; exec cat"};
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

    /* A literal that ignores case: the newest of three is current. */
    uint32_t refusal = 99;
    EXPECT(start(app, id, "needle", 0, &refusal), TESSERA_ERROR_OK);
    CHECK(refusal == TESSERA_SEARCH_REFUSAL_NONE, "refusal %u on success", refusal);
    TesseraSearchStatus status;
    struct timespec begun;
    clock_gettime(CLOCK_MONOTONIC, &begun);
    for (;;) {
        EXPECT(finish(app, id, &status), TESSERA_ERROR_OK);
        if (status.matches == 3) {
            break;
        }
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        CHECK(now.tv_sec - begun.tv_sec <= 5, "timed out with %zu matches", status.matches);
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
    CHECK(status.active && status.done && !status.truncated && !status.lines_skipped,
          "status flags");
    CHECK(status.has_current && status.current == 2, "current %zu", status.current);

    /* Older steps back, newer forward; both wrap. */
    EXPECT(tessera_session_search_older(app, id, &status), TESSERA_ERROR_OK);
    CHECK(status.current == 1, "older gave %zu", status.current);
    EXPECT(tessera_session_search_newer(app, id, &status), TESSERA_ERROR_OK);
    EXPECT(tessera_session_search_newer(app, id, &status), TESSERA_ERROR_OK);
    CHECK(status.current == 0, "newer past the newest gave %zu", status.current);
    EXPECT(tessera_session_search_older(app, id, &status), TESSERA_ERROR_OK);
    CHECK(status.current == 2, "older past the oldest gave %zu", status.current);
    EXPECT(tessera_session_search_status(app, id, &status), TESSERA_ERROR_OK);
    CHECK(status.matches == 3 && status.current == 2, "status after the moves");

    /* Reveal scrolls to the current match (on screen here) and keeps it. */
    status.current = 99;
    EXPECT(tessera_session_search_reveal(app, id, &status), TESSERA_ERROR_OK);
    CHECK(status.matches == 3 && status.has_current && status.current == 2,
          "reveal moved the current match to %zu", status.current);

    /* Copy all matches: a short buffer gets nothing but the size. */
    const char *all = "needle\nNEEDLE\nneedle";
    char buf[256];
    size_t len = 0;
    bool truncated = true;
    memset(buf, '#', sizeof buf);
    EXPECT(tessera_session_search_matches_text(app, id, (uint8_t *)buf, 4, &len, &truncated),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(len == strlen(all) && buf[0] == '#', "short buffer: len %zu", len);
    truncated = true;
    EXPECT(tessera_session_search_matches_text(app, id, (uint8_t *)buf, sizeof buf, &len,
                                               &truncated),
           TESSERA_ERROR_OK);
    CHECK(len == strlen(all) && memcmp(buf, all, len) == 0 && !truncated, "text \"%.*s\"",
          (int)len, buf);
    EXPECT(tessera_session_search_matches_text(app, id, NULL, sizeof buf, &len, NULL),
           TESSERA_ERROR_NULL_POINTER);
    CHECK(len == 0, "NULL buffer left len %zu", len);

    /* A case-sensitive regex: two of the three. */
    EXPECT(start(app, id, "needle \\d", TESSERA_SEARCH_REGEX | TESSERA_SEARCH_CASE_SENSITIVE,
                 &refusal),
           TESSERA_ERROR_OK);
    EXPECT(finish(app, id, &status), TESSERA_ERROR_OK);
    CHECK(status.matches == 2 && status.current == 1, "regex: %zu matches", status.matches);

    /* An invalid regex: refused with a message, the search cleared. */
    char message[512];
    size_t message_len = 0;
    EXPECT(tessera_session_search_start(app, id, (const uint8_t *)"(", 1, TESSERA_SEARCH_REGEX,
                                        &refusal, (uint8_t *)message, sizeof message,
                                        &message_len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(refusal == TESSERA_SEARCH_REFUSAL_SYNTAX, "refusal %u", refusal);
    CHECK(message_len > 0 && message_len <= sizeof message, "message length %zu", message_len);
    EXPECT(tessera_session_search_status(app, id, &status), TESSERA_ERROR_OK);
    CHECK(!status.active && status.matches == 0, "a refused pattern left a search");
    /* A message cut to the buffer. */
    EXPECT(tessera_session_search_start(app, id, (const uint8_t *)"(", 1, TESSERA_SEARCH_REGEX,
                                        &refusal, (uint8_t *)message, 3, &message_len),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(message_len <= 3, "message not cut: %zu", message_len);

    EXPECT(start(app, id, "\\w{100000}", TESSERA_SEARCH_REGEX, &refusal),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(refusal == TESSERA_SEARCH_REFUSAL_TOO_COMPLEX, "refusal %u", refusal);

    /* Too long: refused by its length (the bytes are never read). */
    static char long_pattern[TESSERA_SEARCH_MAX_QUERY_BYTES + 1];
    memset(long_pattern, 'a', sizeof long_pattern);
    EXPECT(tessera_session_search_start(app, id, (const uint8_t *)long_pattern,
                                        TESSERA_SEARCH_MAX_QUERY_BYTES, 0, &refusal, NULL, 0,
                                        NULL),
           TESSERA_ERROR_OK);
    EXPECT(tessera_session_search_start(app, id, (const uint8_t *)long_pattern,
                                        sizeof long_pattern, 0, &refusal, NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(refusal == TESSERA_SEARCH_REFUSAL_TOO_LONG, "refusal %u", refusal);
    EXPECT(tessera_session_search_start(app, id, NULL, (size_t)-1, 0, &refusal, NULL, 0, NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(refusal == TESSERA_SEARCH_REFUSAL_TOO_LONG, "refusal %u", refusal);

    EXPECT(tessera_session_search_start(app, id, (const uint8_t *)"\xff", 1, 0, &refusal, NULL, 0,
                                        NULL),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(refusal == TESSERA_SEARCH_REFUSAL_NOT_UTF8, "refusal %u", refusal);

    /* An unknown option bit changes nothing. */
    EXPECT(start(app, id, "hay", 0, NULL), TESSERA_ERROR_OK);
    EXPECT(start(app, id, "needle", 4, &refusal), TESSERA_ERROR_INVALID_ARGUMENT);
    EXPECT(finish(app, id, &status), TESSERA_ERROR_OK);
    CHECK(status.active && status.matches == 1, "an unknown option changed the search");

    /* Clearing, and an empty pattern, leave no search. */
    EXPECT(tessera_session_search_clear(app, id), TESSERA_ERROR_OK);
    EXPECT(tessera_session_search_status(app, id, &status), TESSERA_ERROR_OK);
    CHECK(!status.active && status.matches == 0 && !status.has_current, "cleared search");
    EXPECT(tessera_session_search_start(app, id, NULL, 0, 0, NULL, NULL, 0, NULL),
           TESSERA_ERROR_OK);
    EXPECT(tessera_session_search_step(app, id, 0, &status), TESSERA_ERROR_OK);
    CHECK(!status.active, "an empty pattern searches");
    EXPECT(tessera_session_search_older(app, id, &status), TESSERA_ERROR_OK);
    CHECK(!status.has_current, "a move without matches");

    /* Bad arguments. */
    EXPECT(tessera_session_search_start(NULL, id, (const uint8_t *)"a", 1, 0, NULL, NULL, 0,
                                        NULL),
           TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_start(app, id, NULL, 3, 0, NULL, NULL, 0, NULL),
           TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_step(app, id, 0, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_step(NULL, id, 0, &status), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_older(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_newer(NULL, id, &status), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_reveal(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_reveal(NULL, id, &status), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_status(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_clear(NULL, id), TESSERA_ERROR_NULL_POINTER);
    EXPECT(tessera_session_search_matches_text(app, id, (uint8_t *)buf, sizeof buf, NULL, NULL),
           TESSERA_ERROR_NULL_POINTER);

    uint64_t unknown = id + 1000;
    EXPECT(start(app, unknown, "a", 0, NULL), TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT(start(app, unknown, "(", TESSERA_SEARCH_REGEX, NULL), TESSERA_ERROR_SESSION_NOT_FOUND);
    status.matches = 7;
    EXPECT(tessera_session_search_step(app, unknown, 0, &status),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    CHECK(status.matches == 0, "status not zeroed after an unknown session");
    EXPECT(tessera_session_search_older(app, unknown, &status), TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT(tessera_session_search_newer(app, unknown, &status), TESSERA_ERROR_SESSION_NOT_FOUND);
    status.matches = 7;
    EXPECT(tessera_session_search_reveal(app, unknown, &status), TESSERA_ERROR_SESSION_NOT_FOUND);
    CHECK(status.matches == 0, "reveal left the status after an unknown session");
    EXPECT(tessera_session_search_status(app, unknown, &status), TESSERA_ERROR_SESSION_NOT_FOUND);
    EXPECT(tessera_session_search_clear(app, unknown), TESSERA_ERROR_SESSION_NOT_FOUND);
    len = 5;
    EXPECT(tessera_session_search_matches_text(app, unknown, (uint8_t *)buf, sizeof buf, &len,
                                               NULL),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    CHECK(len == 0, "len not zeroed after an unknown session");

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
