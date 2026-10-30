/*
 * Clipboard smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh printing one OSC 52 write of
 * "a" NUL "b" ESC "[31mc" LF, wait (at most 5 s) for on_session_clipboard
 * and check it got exactly those 10 bytes (ptr + len: the NUL is data, not
 * a terminator), once, for that session. Also checks the callback sits
 * after on_appearance_changed at the end of TesseraCallbacks.
 */

#include <pthread.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "TesseraCore.h"

/* Appended after on_appearance_changed (#9): the table's last field. */
_Static_assert(offsetof(TesseraCallbacks, on_session_clipboard) ==
                   offsetof(TesseraCallbacks, on_appearance_changed) + sizeof(void (*)(void)),
               "on_session_clipboard follows on_appearance_changed");
_Static_assert(sizeof(TesseraCallbacks) ==
                   offsetof(TesseraCallbacks, on_session_clipboard) + sizeof(void (*)(void)),
               "on_session_clipboard is the last field");

static const unsigned char expected[] = {'a', 0, 'b', 0x1b, '[', '3', '1', 'm', 'c', '\n'};

struct recorder {
    pthread_mutex_t lock;
    pthread_cond_t changed;
    unsigned calls;
    uint64_t id;
    size_t len;
    unsigned char text[64];
};

static void on_clipboard(void *user_data, uint64_t session_id, const uint8_t *text, size_t len) {
    struct recorder *r = user_data;
    pthread_mutex_lock(&r->lock);
    r->calls++;
    r->id = session_id;
    r->len = len;
    memcpy(r->text, text, len < sizeof r->text ? len : sizeof r->text);
    pthread_cond_broadcast(&r->changed);
    pthread_mutex_unlock(&r->lock);
}

#define CHECK(expr)                                                              \
    do {                                                                         \
        TesseraError check_status = (expr);                                      \
        if (check_status != TESSERA_ERROR_OK) {                                  \
            fprintf(stderr, "%s:%d: %s returned %d\n", __FILE__, __LINE__, #expr, \
                    (int)check_status);                                          \
            return 1;                                                            \
        }                                                                        \
    } while (0)

int main(void) {
    struct recorder rec;
    memset(&rec, 0, sizeof rec);
    pthread_mutex_init(&rec.lock, NULL);
    pthread_cond_init(&rec.changed, NULL);

    TesseraApp *app = NULL;
    CHECK(tessera_app_new(&app));
    TesseraCallbacks callbacks = {
        .user_data = &rec,
        .on_session_clipboard = on_clipboard,
    };
    CHECK(tessera_app_set_callbacks(app, &callbacks));

    /* base64 of the 10 bytes in `expected`; `exec cat` keeps the child. */
    const char *args[] = {"-c", "printf '\\033]52;c;YQBiG1szMW1jCg==\\007'; exec cat"};
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
    CHECK(tessera_session_spawn(app, &options, &id));

    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += 5;
    pthread_mutex_lock(&rec.lock);
    while (rec.calls == 0) {
        if (pthread_cond_timedwait(&rec.changed, &rec.lock, &deadline) != 0) {
            break;
        }
    }
    unsigned calls = rec.calls;
    uint64_t got_id = rec.id;
    size_t len = rec.len;
    unsigned char text[sizeof rec.text];
    memcpy(text, rec.text, sizeof text);
    pthread_mutex_unlock(&rec.lock);

    if (calls != 1 || got_id != id) {
        fprintf(stderr, "on_session_clipboard: %u call(s) for session %llu, expected 1 for %llu\n",
                calls, (unsigned long long)got_id, (unsigned long long)id);
        return 1;
    }
    if (len != sizeof expected || memcmp(text, expected, sizeof expected) != 0) {
        fprintf(stderr, "on_session_clipboard: %zu bytes, expected %zu:", len, sizeof expected);
        for (size_t i = 0; i < len && i < sizeof text; i++) {
            fprintf(stderr, " %02x", text[i]);
        }
        fprintf(stderr, "\n");
        return 1;
    }

    CHECK(tessera_session_close(app, id));
    tessera_app_free(app);
    pthread_mutex_destroy(&rec.lock);
    pthread_cond_destroy(&rec.changed);
    printf("clipboard: %zu bytes with NUL and ESC received\n", len);
    return 0;
}
