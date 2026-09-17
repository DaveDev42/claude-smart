/* Fake `claude` binary for csm's e2e limit-switch harness.
 *
 * On start: appends an invocation record (pid, ppid, CLAUDE_CONFIG_DIR, full
 * argv) to the file named by env FAKE_LOG. Then blocks until SIGTERM, at
 * which point it exits(0). Never does anything else -- no real claude ever
 * runs.
 *
 * POSIX-only (fork/signal semantics the harness depends on: SIGTERM delivery,
 * pause()). Built fresh by e2e/run.sh with `cc -std=c11 -Wall -Wextra`; must
 * stay warning-free on both gcc and clang.
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <string.h>
#include <time.h>

static volatile sig_atomic_t got_term = 0;

static void on_term(int sig) {
    (void)sig;
    got_term = 1;
}

int main(int argc, char **argv) {
    const char *log_path = getenv("FAKE_LOG");
    if (log_path) {
        FILE *f = fopen(log_path, "a");
        if (f) {
            time_t now = time(NULL);
            const char *cfgdir = getenv("CLAUDE_CONFIG_DIR");
            fprintf(f, "=== INVOCATION pid=%d ppid=%d time=%ld config_dir=%s ===\n",
                    (int)getpid(), (int)getppid(), (long)now, cfgdir ? cfgdir : "(unset)");
            for (int i = 0; i < argc; i++) {
                fprintf(f, "argv[%d]=%s\n", i, argv[i]);
            }
            fprintf(f, "=== END ===\n");
            fflush(f);
            fclose(f);
        }
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_term;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGTERM, &sa, NULL);

    while (!got_term) {
        pause();
    }

    if (log_path) {
        FILE *f = fopen(log_path, "a");
        if (f) {
            fprintf(f, "=== SIGTERM pid=%d exiting ===\n", (int)getpid());
            fclose(f);
        }
    }

    return 0;
}
