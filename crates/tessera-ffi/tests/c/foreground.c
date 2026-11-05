/*
 * Foreground-job smoke test of TesseraCore.h from C, built and run by
 * scripts/ffi-c-test.sh: an interactive /bin/sh (started with ENV=/dev/null,
 * so no startup file of the user's runs, and the prompt PROMPT) reports no
 * job once its prompt is on the screen, `sleep 100`
 * typed into it is reported as running "sleep" (polled, at most 1000 polls
 * 10 ms apart), Ctrl-C ends it, and NULL pointers, an unknown session and a
 * closed one fail with *out zeroed.
 */

#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
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

/* The shell's prompt (PS1), which wait_for_prompt looks for. */
#define PROMPT "tessera-ffi-ready$ "

/* Polls (at most 1000 polls 10 ms apart) until the shell's prompt is on the
 * screen, read through a select-all; returns 0 on success. */
static int wait_for_prompt(TesseraApp *app, uint64_t id) {
    static char text[64 * 1024];
    size_t len = 0;
    for (int attempt = 0; attempt < 1000; attempt++) {
        if (tessera_session_select_all(app, id) != TESSERA_ERROR_OK ||
            tessera_session_selection_text(app, id, (uint8_t *)text, sizeof text - 1, &len) !=
                TESSERA_ERROR_OK) {
            fprintf(stderr, "reading the session's text failed\n");
            return 1;
        }
        text[len] = '\0';
        /* The selection drops the blank after the prompt. */
        if (strstr(text, "tessera-ffi-ready$") != NULL) {
            return 0;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
    fprintf(stderr, "no prompt; the session shows \"%s\"\n", text);
    return 1;
}

/* Whether every byte of *job is zero, as a failed or idle call leaves it. */
static int is_zeroed(const TesseraForegroundProcess *job) {
    if (job->running || job->pid != 0) {
        return 0;
    }
    for (size_t i = 0; i < sizeof job->name; i++) {
        if (job->name[i] != 0) {
            return 0;
        }
    }
    return 1;
}

/* Polls until the session's job is running with `name` (or, for NULL, until
 * none runs); returns 0 on success. */
static int wait_for_job(TesseraApp *app, uint64_t id, const char *name,
                        TesseraForegroundProcess *job) {
    for (int attempt = 0; attempt < 1000; attempt++) {
        if (tessera_session_foreground_process(app, id, job) != TESSERA_ERROR_OK) {
            fprintf(stderr, "tessera_session_foreground_process failed\n");
            return 1;
        }
        if (name == NULL ? !job->running : (job->running && strcmp(job->name, name) == 0)) {
            return 0;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
    fprintf(stderr, "still running=%d pid=%d name=\"%s\", expected %s\n", (int)job->running,
            (int)job->pid, job->name, name == NULL ? "no job" : name);
    return 1;
}

int main(void) {
    if (setenv("ENV", "/dev/null", 1) != 0 || setenv("PS1", PROMPT, 1) != 0) {
        perror("setenv");
        return 1;
    }
    TesseraApp *app = NULL;
    EXPECT(tessera_app_new(&app), TESSERA_ERROR_OK);

    const char *args[] = {"-i"};
    TesseraSpawnOptions options = {
        .program = "/bin/sh",
        .args = args,
        .arg_count = 1,
        .cwd = NULL,
        .rows = 24,
        .cols = 80,
        .width_px = 0,
        .height_px = 0,
    };
    uint64_t id = 0;
    EXPECT(tessera_session_spawn(app, &options, &id), TESSERA_ERROR_OK);

    if (wait_for_prompt(app, id) != 0) {
        return 1;
    }
    TesseraForegroundProcess job;
    memset(&job, 0x55, sizeof job);
    EXPECT(tessera_session_foreground_process(app, id, &job), TESSERA_ERROR_OK);
    if (!is_zeroed(&job)) {
        fprintf(stderr, "a shell at its prompt must report a zeroed job\n");
        return 1;
    }

    const char *command = "sleep 100\n";
    EXPECT(tessera_session_write(app, id, (const uint8_t *)command, strlen(command)),
           TESSERA_ERROR_OK);
    if (wait_for_job(app, id, "sleep", &job) != 0) {
        return 1;
    }
    if (job.pid <= 0 || job.name[TESSERA_FOREGROUND_NAME_LEN] != 0) {
        fprintf(stderr, "bad job: pid %d\n", (int)job.pid);
        return 1;
    }

    /* NULL pointers and an unknown session; *out is zeroed on failure. */
    EXPECT(tessera_session_foreground_process(app, id, NULL), TESSERA_ERROR_NULL_POINTER);
    memset(&job, 0x55, sizeof job);
    EXPECT(tessera_session_foreground_process(NULL, id, &job), TESSERA_ERROR_NULL_POINTER);
    if (!is_zeroed(&job)) {
        fprintf(stderr, "job not zeroed after a NULL app\n");
        return 1;
    }
    memset(&job, 0x55, sizeof job);
    EXPECT(tessera_session_foreground_process(app, id + 1000, &job),
           TESSERA_ERROR_SESSION_NOT_FOUND);
    if (!is_zeroed(&job)) {
        fprintf(stderr, "job not zeroed after an unknown session\n");
        return 1;
    }

    /* Ctrl-C ends the job; the shell takes the terminal back. */
    EXPECT(tessera_session_write(app, id, (const uint8_t *)"\x03", 1), TESSERA_ERROR_OK);
    if (wait_for_job(app, id, NULL, &job) != 0) {
        return 1;
    }

    EXPECT(tessera_session_close(app, id), TESSERA_ERROR_OK);
    EXPECT(tessera_session_foreground_process(app, id, &job), TESSERA_ERROR_SESSION_NOT_FOUND);
    tessera_app_free(app);
    printf("ok\n");
    return 0;
}
