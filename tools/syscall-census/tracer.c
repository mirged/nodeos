/* Syscall census tracer: run a program, follow every fork/vfork/clone, and
 * print "<number> <count>" for each Linux syscall made, to stderr. Used by
 * census.sh; see docs/MUSL.md. */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <sys/user.h>
#include <sys/syscall.h>

static unsigned long counts[512];
int main(int argc, char **argv) {
    pid_t child = fork();
    if (child == 0) { ptrace(PTRACE_TRACEME, 0, 0, 0); execv(argv[1], argv + 1); _exit(127); }
    int status; waitpid(child, &status, 0);
    ptrace(PTRACE_SETOPTIONS, child, 0, PTRACE_O_TRACESYSGOOD|PTRACE_O_TRACEFORK|PTRACE_O_TRACEVFORK|PTRACE_O_TRACECLONE|PTRACE_O_EXITKILL);
    ptrace(PTRACE_SYSCALL, child, 0, 0);
    int live = 1; static char entering[1<<22];
    while (live > 0) {
        pid_t p = waitpid(-1, &status, __WALL);
        if (p < 0) break;
        if (WIFEXITED(status) || WIFSIGNALED(status)) { live--; continue; }
        int sig = 0;
        if (WIFSTOPPED(status)) {
            int s = WSTOPSIG(status);
            if (s == (SIGTRAP|0x80)) {
                struct user_regs_struct r; ptrace(PTRACE_GETREGS, p, 0, &r);
                entering[p] ^= 1;
                if (entering[p] && r.orig_rax < 512) counts[r.orig_rax]++;
            } else if (s == SIGTRAP && (status >> 16)) { live++; }
            else if (s != SIGSTOP && s != SIGTRAP) sig = s;
        }
        ptrace(PTRACE_SYSCALL, p, 0, sig);
    }
    for (int i = 0; i < 512; i++) if (counts[i]) fprintf(stderr, "%d %lu\n", i, counts[i]);
}
