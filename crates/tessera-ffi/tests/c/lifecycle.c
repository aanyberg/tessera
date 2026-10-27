/*
 * Lifecycle smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: create an application, spawn /bin/sh, write
 * "exit\n", wait (at most 5 s) for on_session_exit with code 0, then free.
 */

#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "TesseraCore.h"

struct recorder {
    pthread_mutex_t lock;
    pthread_cond_t changed;
    unsigned outputs;
    int exited;
    uint64_t exit_id;
    int32_t code;
    int32_t signal;
};

static void on_output(void *user_data, uint64_t session_id) {
    struct recorder *r = user_data;
    (void)session_id;
    pthread_mutex_lock(&r->lock);
    r->outputs++;
    pthread_cond_broadcast(&r->changed);
    pthread_mutex_unlock(&r->lock);
}

static void on_exit_cb(void *user_data, uint64_t session_id, int32_t code, int32_t signal) {
    struct recorder *r = user_data;
    pthread_mutex_lock(&r->lock);
    r->exited = 1;
    r->exit_id = session_id;
    r->code = code;
    r->signal = signal;
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

    printf("tessera core %s\n", tessera_version());

    TesseraApp *app = NULL;
    CHECK(tessera_app_new(&app));

    TesseraCallbacks callbacks = {
        .user_data = &rec,
        .on_session_output = on_output,
        .on_session_title = NULL,
        .on_session_bell = NULL,
        .on_session_exit = on_exit_cb,
    };
    CHECK(tessera_app_set_callbacks(app, &callbacks));

    size_t path_len = 0;
    CHECK(tessera_app_config_path(app, NULL, 0, &path_len));
    char path[4096];
    if (path_len >= sizeof path) {
        fprintf(stderr, "config path unexpectedly long (%zu bytes)\n", path_len);
        return 1;
    }
    CHECK(tessera_app_config_path(app, path, sizeof path, NULL));
    printf("config path: %s\n", path);

    if (tessera_session_close(app, 12345) != TESSERA_ERROR_SESSION_NOT_FOUND) {
        fprintf(stderr, "closing an unknown session should fail\n");
        return 1;
    }

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
    CHECK(tessera_session_spawn(app, &options, &id));
    CHECK(tessera_session_resize(app, id, 30, 100, 0, 0));
    static const char input[] = "exit\n";
    CHECK(tessera_session_write(app, id, (const uint8_t *)input, strlen(input)));

    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += 5;
    pthread_mutex_lock(&rec.lock);
    int wait_status = 0;
    while (!rec.exited && wait_status == 0) {
        wait_status = pthread_cond_timedwait(&rec.changed, &rec.lock, &deadline);
    }
    int exited = rec.exited;
    uint64_t exit_id = rec.exit_id;
    int32_t code = rec.code;
    int32_t signal = rec.signal;
    unsigned outputs = rec.outputs;
    pthread_mutex_unlock(&rec.lock);

    if (!exited) {
        fprintf(stderr, "timed out waiting for on_session_exit\n");
        return 1;
    }
    if (exit_id != id || code != 0 || signal != 0) {
        fprintf(stderr, "unexpected exit: session %llu code %d signal %d\n",
                (unsigned long long)exit_id, code, signal);
        return 1;
    }
    printf("session %llu exited with code %d after %u output callbacks\n",
           (unsigned long long)id, code, outputs);

    CHECK(tessera_session_close(app, id));
    tessera_app_free(app);
    pthread_cond_destroy(&rec.changed);
    pthread_mutex_destroy(&rec.lock);
    printf("ok\n");
    return 0;
}
