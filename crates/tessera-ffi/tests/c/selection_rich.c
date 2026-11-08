/*
 * Copy with Formatting and smart selection from C, built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh printing a red word, hostile text
 * and an email address, wait (at most 5 s) until Select All copies them, then copy
 * the selection as HTML, RTF and plain text, with short buffers, NULL
 * buffers, no selection, a NULL handle and an unknown session id, and
 * double-click the address (smart selection) and a plain word.
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
            fputc('\n', stderr);                                                     \
            return 1;                                                                \
        }                                                                            \
    } while (0)

/* Whether `len` bytes at `buf` contain the NUL-terminated `needle`. */
static int contains(const char *buf, size_t len, const char *needle) {
    size_t n = strlen(needle);
    for (size_t i = 0; i + n <= len; i++) {
        if (memcmp(buf + i, needle, n) == 0) {
            return 1;
        }
    }
    return 0;
}

/* Whether all `len` bytes at `buf` are `#`. */
static int untouched(const char *buf, size_t len) {
    for (size_t i = 0; i < len; i++) {
        if (buf[i] != '#') {
            return 0;
        }
    }
    return 1;
}

static char html_buf[4096], rtf_buf[4096], plain_buf[4096];
static TesseraTextBuffer html, rtf, plain;

/* Resets the three buffers to `#` with the given capacities. */
static void reset(size_t html_cap, size_t rtf_cap, size_t plain_cap) {
    memset(html_buf, '#', sizeof html_buf);
    memset(rtf_buf, '#', sizeof rtf_buf);
    memset(plain_buf, '#', sizeof plain_buf);
    html = (TesseraTextBuffer){.buf = (uint8_t *)html_buf, .cap = html_cap, .len = 99};
    rtf = (TesseraTextBuffer){.buf = (uint8_t *)rtf_buf, .cap = rtf_cap, .len = 99};
    plain = (TesseraTextBuffer){.buf = (uint8_t *)plain_buf, .cap = plain_cap, .len = 99};
}

int main(void) {
    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    const char *args[] = {
        "-c", "printf '\\033[1;31mred\\033[0m <a href=x>&{}\\\\\\nsee me@example.com now\\n'; "
              "exec cat"};
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

    const char *all = "red <a href=x>&{}\\\nsee me@example.com now";
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        EXPECT(tessera_session_select_all(app, id), TESSERA_ERROR_OK);
        reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
        EXPECT(tessera_session_selection_rich_text(app, id, &html, &rtf, &plain),
               TESSERA_ERROR_OK);
        if (plain.len == strlen(all) && memcmp(plain_buf, all, plain.len) == 0) {
            break;
        }
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        CHECK(now.tv_sec - start.tv_sec <= 5, "timed out waiting for the output (\"%.*s\")",
              (int)plain.len, plain_buf);
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }

    /* The colour and weight are there, the hostile text escaped, no link. */
    CHECK(contains(html_buf, html.len, "font-weight:bold"), "html: %.*s", (int)html.len,
          html_buf);
    CHECK(contains(html_buf, html.len, "&lt;a href=x&gt;&amp;&#123;&#125;&#92;"), "html: %.*s",
          (int)html.len, html_buf);
    CHECK(!contains(html_buf, html.len, "<a "), "html has a link: %.*s", (int)html.len,
          html_buf);
    CHECK(rtf.len > 6 && memcmp(rtf_buf, "{\\rtf1", 6) == 0, "rtf: %.*s", (int)rtf.len, rtf_buf);
    CHECK(contains(rtf_buf, rtf.len, "\\{\\}\\\\"), "rtf: %.*s", (int)rtf.len, rtf_buf);
    CHECK(rtf_buf[rtf.len - 1] == '}' && rtf_buf[rtf.len] == '#', "rtf end");
    size_t need_html = html.len, need_rtf = rtf.len, need_plain = plain.len;

    /* A short buffer: every size, nothing written. */
    reset(need_html, need_rtf - 1, need_plain);
    EXPECT(tessera_session_selection_rich_text(app, id, &html, &rtf, &plain),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(html.len == need_html && rtf.len == need_rtf && plain.len == need_plain,
          "short: lens %zu %zu %zu", html.len, rtf.len, plain.len);
    CHECK(untouched(html_buf, sizeof html_buf) && untouched(rtf_buf, sizeof rtf_buf) &&
              untouched(plain_buf, sizeof plain_buf),
          "a short buffer wrote something");

    /* NULL buffers with no capacity ask for the sizes. */
    TesseraTextBuffer none[3] = {{NULL, 0, 99}, {NULL, 0, 99}, {NULL, 0, 99}};
    EXPECT(tessera_session_selection_rich_text(app, id, &none[0], &none[1], &none[2]),
           TESSERA_ERROR_INVALID_ARGUMENT);
    CHECK(none[0].len == need_html && none[1].len == need_rtf && none[2].len == need_plain,
          "size query: lens %zu %zu %zu", none[0].len, none[1].len, none[2].len);

    /* A NULL buffer claiming room, a NULL struct, a NULL handle, an unknown id. */
    reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
    plain.buf = NULL;
    EXPECT(tessera_session_selection_rich_text(app, id, &html, &rtf, &plain),
           TESSERA_ERROR_NULL_POINTER);
    CHECK(html.len == 0 && rtf.len == 0 && plain.len == 0, "NULL buf: lens not reset");
    CHECK(untouched(html_buf, sizeof html_buf), "NULL buf: html written");
    reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
    EXPECT(tessera_session_selection_rich_text(app, id, &html, NULL, &plain),
           TESSERA_ERROR_NULL_POINTER);
    CHECK(html.len == 0 && plain.len == 0, "NULL rtf: lens not reset");
    reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
    EXPECT(tessera_session_selection_rich_text(NULL, id, &html, &rtf, &plain),
           TESSERA_ERROR_NULL_POINTER);
    CHECK(html.len == 0 && rtf.len == 0 && plain.len == 0, "NULL app: lens not reset");
    reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
    EXPECT(tessera_session_selection_rich_text(app, 0xDEADBEEF, &html, &rtf, &plain),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    CHECK(html.len == 0 && rtf.len == 0 && plain.len == 0, "unknown id: lens not reset");

    /* Smart selection: a double-click on the domain selects the whole
     * address (word selection would stop at the `@`). */
    EXPECT(tessera_session_select_begin(app, id, 12, 1, TESSERA_GRANULARITY_WORD, false),
           TESSERA_ERROR_OK);
    reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
    EXPECT(tessera_session_selection_rich_text(app, id, &html, &rtf, &plain), TESSERA_ERROR_OK);
    CHECK(plain.len == strlen("me@example.com") &&
              memcmp(plain_buf, "me@example.com", plain.len) == 0,
          "email double-click: \"%.*s\"", (int)plain.len, plain_buf);
    /* A plain word is still a word. */
    EXPECT(tessera_session_select_begin(app, id, 1, 1, TESSERA_GRANULARITY_WORD, false),
           TESSERA_ERROR_OK);
    reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
    EXPECT(tessera_session_selection_rich_text(app, id, &html, &rtf, &plain), TESSERA_ERROR_OK);
    CHECK(plain.len == 3 && memcmp(plain_buf, "see", 3) == 0, "word double-click: \"%.*s\"",
          (int)plain.len, plain_buf);

    /* No selection: Ok, every length 0. */
    EXPECT(tessera_session_select_clear(app, id), TESSERA_ERROR_OK);
    reset(sizeof html_buf, sizeof rtf_buf, sizeof plain_buf);
    EXPECT(tessera_session_selection_rich_text(app, id, &html, &rtf, &plain), TESSERA_ERROR_OK);
    CHECK(html.len == 0 && rtf.len == 0 && plain.len == 0, "no selection: lens %zu %zu %zu",
          html.len, rtf.len, plain.len);

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
