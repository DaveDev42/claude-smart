/* Fake `claude` (and fake Orca main process) for csm's e2e harness.
 *
 * Built once per run by e2e/run.sh with `cc -std=c11 -Wall -Wextra`; must
 * stay warning-free on gcc and clang. One binary plays every part, chosen
 * by environment and argv, so no scenario ever builds or writes another
 * executable:
 *
 * - FAKE_ORCA_HOLDER=<userData>: the fake Orca's main process. It points
 *   <userData>/SingletonLock at "<hostname>-<pid>" the way Chromium does,
 *   waits for SIGTERM, then removes the lock. run.sh hard-links this binary
 *   to an `Orca`/`orca-ide` path so csm's main-executable match accepts it.
 * - `--version`: print a Claude Code version line and exit.
 * - `auth status --json`: print a logged-in status object and exit.
 * - `-p` / `--print` before `--`, `mcp` as the first word, or a stdin that
 *   is not a terminal: print one line, log a CALL record and exit (the Print
 *   and passthrough paths; supervised launches run under `script`, so their
 *   stdin is a pty).
 * - anything else: an interactive session. Append an INVOCATION record
 *   (pid, ppid, CLAUDE_CONFIG_DIR, argv) to FAKE_LOG, then block until
 *   SIGTERM and log that too.
 *
 * With FAKE_EXPECT=<path> every record also says whether that path existed
 * when claude started (`expect=present|absent`): how a scenario proves a
 * file was in place before the spawn (a resumed transcript, say), and
 * with FAKE_COUNT=<path> how many lines that file had then (`count=N`):
 * how a scenario proves which requests reached the fake Orca before the
 * spawn.
 *
 * Records never carry credentials; the harness's fixtures hold only fake
 * tokens anyway.
 *
 * -std=c11 asks for strict ISO C, and under it glibc hides what POSIX adds
 * unless a feature-test macro asks for it (sigaction, gethostname, symlink).
 */
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE 1
#define _DARWIN_C_SOURCE 1

#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#ifndef HOST_NAME_MAX
#define HOST_NAME_MAX 255
#endif

static volatile sig_atomic_t got_term = 0;

static void on_term(int sig) {
    (void)sig;
    got_term = 1;
}

static void wait_for_term(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_term;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGHUP, &sa, NULL);
    while (!got_term) {
        pause();
    }
}

/* Lines in a file; 0 when it cannot be read. */
static long count_lines(const char *path) {
    FILE *f = fopen(path, "r");
    if (!f) {
        return 0;
    }
    long n = 0;
    int c;
    while ((c = fgetc(f)) != EOF) {
        if (c == '\n') {
            n++;
        }
    }
    fclose(f);
    return n;
}

static void log_record(const char *kind, int argc, char **argv) {
    const char *log_path = getenv("FAKE_LOG");
    if (!log_path || !*log_path) {
        return;
    }
    FILE *f = fopen(log_path, "a");
    if (!f) {
        return;
    }
    const char *cfgdir = getenv("CLAUDE_CONFIG_DIR");
    const char *expect = getenv("FAKE_EXPECT");
    const char *seen = "";
    if (expect && *expect) {
        seen = access(expect, F_OK) == 0 ? " expect=present" : " expect=absent";
    }
    char counted[48] = "";
    const char *count_path = getenv("FAKE_COUNT");
    if (count_path && *count_path) {
        snprintf(counted, sizeof(counted), " count=%ld", count_lines(count_path));
    }
    fprintf(f, "=== %s pid=%d ppid=%d time=%ld config_dir=%s%s%s ===\n", kind, (int)getpid(),
            (int)getppid(), (long)time(NULL), cfgdir ? cfgdir : "(unset)", seen, counted);
    for (int i = 0; i < argc; i++) {
        fprintf(f, "argv[%d]=%s\n", i, argv[i]);
    }
    fprintf(f, "=== END ===\n");
    fclose(f);
}

static int holder(const char *user_data) {
    char host[HOST_NAME_MAX + 1];
    if (gethostname(host, sizeof(host)) != 0) {
        return 3;
    }
    host[HOST_NAME_MAX] = '\0';
    char target[HOST_NAME_MAX + 32];
    snprintf(target, sizeof(target), "%s-%d", host, (int)getpid());
    char lock[PATH_MAX];
    snprintf(lock, sizeof(lock), "%s/SingletonLock", user_data);
    unlink(lock);
    if (symlink(target, lock) != 0) {
        return 4;
    }
    wait_for_term();
    unlink(lock);
    return 0;
}

int main(int argc, char **argv) {
    const char *hold = getenv("FAKE_ORCA_HOLDER");
    if (hold && *hold) {
        return holder(hold);
    }

    if (argc >= 2 && strcmp(argv[1], "--version") == 0) {
        printf("2.1.283 (Claude Code)\n");
        return 0;
    }
    if (argc >= 3 && strcmp(argv[1], "auth") == 0 && strcmp(argv[2], "status") == 0) {
        printf("{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}\n");
        return 0;
    }
    int print = (argc >= 2 && strcmp(argv[1], "mcp") == 0) || !isatty(STDIN_FILENO);
    for (int i = 1; i < argc && !print; i++) {
        if (strcmp(argv[i], "--") == 0) {
            break;
        }
        print = strcmp(argv[i], "-p") == 0 || strcmp(argv[i], "--print") == 0;
    }
    if (print) {
        log_record("CALL", argc, argv);
        printf("fake claude: done\n");
        return 0;
    }

    log_record("INVOCATION", argc, argv);
    wait_for_term();
    const char *log_path = getenv("FAKE_LOG");
    if (log_path && *log_path) {
        FILE *f = fopen(log_path, "a");
        if (f) {
            fprintf(f, "=== SIGTERM pid=%d exiting ===\n", (int)getpid());
            fclose(f);
        }
    }
    return 0;
}
