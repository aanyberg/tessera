/*
 * Remote-host smoke test of TesseraCore.h from C (#205), built and run by
 * scripts/ffi-c-test.sh: spawn /bin/sh printing an OSC 7 report from
 * another machine, wait (at most 5 s) for on_session_remote and check it
 * got TESSERA_REMOTE_KIND_HOST with exactly the host's bytes (ptr + len,
 * not NUL-terminated), once, for that session. Also checks the callback
 * is the table's last field, right after on_config_applied (S7).
 */

#include <pthread.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "TesseraCore.h"

_Static_assert(offsetof(TesseraCallbacks, on_session_remote) ==
                   offsetof(TesseraCallbacks, on_config_applied) + sizeof(void (*)(void)),
               "on_session_remote follows on_config_applied");
_Static_assert(sizeof(TesseraCallbacks) ==
                   offsetof(TesseraCallbacks, on_session_remote) + sizeof(void (*)(void)),
               "on_session_remote is the last field");
_Static_assert(TESSERA_MAX_REMOTE_HOST_LEN == 253, "the longest DNS name");

static const char expected[] = "far.example";

struct recorder {
    pthread_mutex_t lock;
    pthread_cond_t changed;
    unsigned calls;
    uint64_t id;
    uint8_t kind;
    size_t len;
    char host[TESSERA_MAX_REMOTE_HOST_LEN];
};

static void on_remote(void *user_data, uint64_t session_id, uint8_t kind, const uint8_t *host,
                      size_t len) {
    struct recorder *r = user_data;
    pthread_mutex_lock(&r->lock);
    r->calls++;
    r->id = session_id;
    r->kind = kind;
    r->len = len;
    if (host != NULL) {
        memcpy(r->host, host, len < sizeof r->host ? len : sizeof r->host);
    }
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
        .on_session_remote = on_remote,
    };
    CHECK(tessera_app_set_callbacks(app, &callbacks));

    /* `exec cat` keeps the child, so no exit races the report. */
    const char *args[] = {"-c", "printf '\\033]7;file://far.example/srv\\007'; exec cat"};
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
    uint8_t kind = rec.kind;
    size_t len = rec.len;
    char host[sizeof rec.host];
    memcpy(host, rec.host, sizeof host);
    pthread_mutex_unlock(&rec.lock);

    if (calls != 1 || got_id != id || kind != TESSERA_REMOTE_KIND_HOST) {
        fprintf(stderr, "on_session_remote: %u call(s) of kind %u for session %llu, expected 1 of %u for %llu\n",
                calls, (unsigned)kind, (unsigned long long)got_id,
                (unsigned)TESSERA_REMOTE_KIND_HOST, (unsigned long long)id);
        return 1;
    }
    if (len != strlen(expected) || memcmp(host, expected, len) != 0) {
        fprintf(stderr, "on_session_remote: host of %zu bytes, expected \"%s\"\n", len, expected);
        return 1;
    }

    CHECK(tessera_session_close(app, id));
    tessera_app_free(app);
    pthread_mutex_destroy(&rec.lock);
    pthread_cond_destroy(&rec.changed);
    printf("remote: host \"%.*s\" received\n", (int)len, host);
    return 0;
}
