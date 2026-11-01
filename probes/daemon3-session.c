/*
 * What glibc's daemon(3) actually does to a process, measured rather than
 * quoted from the man page.
 *
 * daemon(3) forks once and calls setsid(2). The consequence that matters is not
 * the fork count but the result: the process that survives is a *session
 * leader*. A session leader that opens a terminal acquires it as a controlling
 * terminal, which is exactly the property a daemon must not have.
 *
 * Build and run:
 *
 *   cc -O0 -o /tmp/dprobe probes/daemon3-session.c
 *   /tmp/dprobe /tmp/probe.out && cat /tmp/probe.out
 *
 * Prints "<pid> <sid>". When they are equal, the daemon is a session leader.
 * d-mon's supervisor, for comparison, is neither a session leader nor a process
 * group leader: both belong to the intermediate process, which exits.
 */
#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) {
        return 2;
    }
    if (daemon(0, 0) != 0) {
        return 1;
    }
    /* Both descriptors point at /dev/null by now, so report through a file. */
    FILE *f = fopen(argv[1], "w");
    if (!f) {
        return 3;
    }
    fprintf(f, "%ld %ld\n", (long)getpid(), (long)getsid(0));
    fclose(f);
    /* Stay alive long enough for the parent's exit status to be observed. */
    sleep(2);
    return 0;
}