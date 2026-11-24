/*
 * grantpt_stress.c: opens and closes ptys from many threads as fast as they
 * can, and reports a thread whose open stops returning (#568).
 *
 * On macOS a `grantpt(3)` (which `openpty(3)` calls) sometimes spins in the
 * kernel and never returns while other threads open and close ptys at the
 * same time: about once per 170,000 to 600,000 opens at 16 threads (#563,
 * #568). It happens with and without a lock around the open, since closes
 * race with opens. Only a signal ends the stuck thread. This harness is the
 * reproduction for a report to Apple (Feedback Assistant); it is not built
 * or run by the test suite.
 *
 * Build and run (macOS):
 *
 *   cc -O2 -Wall -Wextra -pthread -o grantpt_stress \
 *       crates/tessera-pty/tests/c/grantpt_stress.c
 *   ./grantpt_stress [threads] [seconds] [stuck-seconds] [openpty]
 *
 * Defaults: 16 threads, 600 seconds, a thread is stuck after 5 seconds
 * without finishing an open. With the fourth argument `openpty` each thread
 * calls `openpty(3)` instead of the steps it is made of (`posix_openpt`,
 * `grantpt`, `unlockpt`, `ptsname_r`, `open` of the slave).
 *
 * Every second it prints the opens so far. A stuck thread is reported with
 * the step it is in (`stuck: thread 3 in grantpt for 5 s after 412345
 * opens`); the harness then exits with status 2, leaving the stuck thread to
 * the process exit. `sample <pid>` or `spindump` taken before then shows the
 * kernel stack. It exits with status 0 when the time is up and no open got
 * stuck, and 1 on an unexpected error (descriptors or ptys exhausted: raise
 * `ulimit -n` or lower the thread count).
 */

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <util.h>

#define MAX_THREADS 256

/* Where a thread is, for the report of a stuck one. */
enum step {
    STEP_IDLE,
    STEP_POSIX_OPENPT,
    STEP_GRANTPT,
    STEP_UNLOCKPT,
    STEP_PTSNAME,
    STEP_OPEN_SLAVE,
    STEP_OPENPTY,
    STEP_CLOSE,
};

static const char *const step_names[] = {
    "idle", "posix_openpt", "grantpt", "unlockpt", "ptsname_r", "open(slave)", "openpty", "close",
};

struct worker {
    pthread_t thread;
    int index;
    _Atomic int step;
    /* Opens this thread finished. */
    _Atomic unsigned long opens;
};

static struct worker workers[MAX_THREADS];
static int use_openpty;
static atomic_bool stop;

static void fail(const char *what, int err) {
    fprintf(stderr, "error: %s: %s (%d)\n", what, strerror(err), err);
    exit(1);
}

/* Retries an open that lost the kernel's pty allocation race: XNU reports
 * it to user space as errno -6 (EREDRIVEOPEN, #563). */
static int lost_race(int err) { return err == -6; }

static void open_once(struct worker *w) {
    int master = -1;
    int slave = -1;
    if (use_openpty) {
        atomic_store(&w->step, STEP_OPENPTY);
        if (openpty(&master, &slave, NULL, NULL, NULL) != 0) {
            if (lost_race(errno)) {
                return;
            }
            fail("openpty", errno);
        }
    } else {
        char name[128];
        atomic_store(&w->step, STEP_POSIX_OPENPT);
        master = posix_openpt(O_RDWR | O_NOCTTY);
        if (master < 0) {
            if (lost_race(errno)) {
                return;
            }
            fail("posix_openpt", errno);
        }
        atomic_store(&w->step, STEP_GRANTPT);
        if (grantpt(master) != 0) {
            fail("grantpt", errno);
        }
        atomic_store(&w->step, STEP_UNLOCKPT);
        if (unlockpt(master) != 0) {
            fail("unlockpt", errno);
        }
        atomic_store(&w->step, STEP_PTSNAME);
        if (ptsname_r(master, name, sizeof name) != 0) {
            fail("ptsname_r", errno);
        }
        atomic_store(&w->step, STEP_OPEN_SLAVE);
        slave = open(name, O_RDWR | O_NOCTTY);
        if (slave < 0) {
            fail("open(slave)", errno);
        }
    }
    atomic_store(&w->step, STEP_CLOSE);
    close(slave);
    close(master);
    atomic_store(&w->step, STEP_IDLE);
    atomic_fetch_add(&w->opens, 1);
}

static void *run(void *arg) {
    struct worker *w = arg;
    while (!atomic_load(&stop)) {
        open_once(w);
    }
    return NULL;
}

static unsigned long total_opens(int threads) {
    unsigned long total = 0;
    for (int i = 0; i < threads; i++) {
        total += atomic_load(&workers[i].opens);
    }
    return total;
}

int main(int argc, char **argv) {
    int threads = argc > 1 ? atoi(argv[1]) : 16;
    int seconds = argc > 2 ? atoi(argv[2]) : 600;
    int stuck_after = argc > 3 ? atoi(argv[3]) : 5;
    use_openpty = argc > 4 && strcmp(argv[4], "openpty") == 0;
    if (threads < 1 || threads > MAX_THREADS || seconds < 1 || stuck_after < 1) {
        fprintf(stderr, "usage: %s [threads 1..%d] [seconds] [stuck-seconds] [openpty]\n", argv[0],
                MAX_THREADS);
        return 1;
    }

    for (int i = 0; i < threads; i++) {
        workers[i].index = i;
        int err = pthread_create(&workers[i].thread, NULL, run, &workers[i]);
        if (err != 0) {
            fail("pthread_create", err);
        }
    }

    /* Opens of each thread when it last made progress, and since when it
     * has made none. */
    unsigned long seen[MAX_THREADS] = {0};
    int still[MAX_THREADS] = {0};
    for (int second = 1; second <= seconds; second++) {
        sleep(1);
        for (int i = 0; i < threads; i++) {
            unsigned long opens = atomic_load(&workers[i].opens);
            if (opens != seen[i]) {
                seen[i] = opens;
                still[i] = 0;
                continue;
            }
            if (++still[i] >= stuck_after) {
                int step = atomic_load(&workers[i].step);
                printf("stuck: thread %d in %s for %d s after %lu opens (pid %d)\n", i,
                       step_names[step], still[i], total_opens(threads), (int)getpid());
                fflush(stdout);
                return 2;
            }
        }
        printf("%d s: %lu opens\n", second, total_opens(threads));
        fflush(stdout);
    }

    atomic_store(&stop, 1);
    for (int i = 0; i < threads; i++) {
        pthread_join(workers[i].thread, NULL);
    }
    printf("done: %lu opens, none stuck\n", total_opens(threads));
    return 0;
}
