/*
 * A parent starts a worker and sleeps until the worker exits. Its
 * SIGCHLD handler sets a flag; the parent checks the flag, logs that it
 * is waiting, then pauses. A worker that exits after the check and
 * before the pause lands its signal in that gap: the handler runs, the
 * parent pauses anyway, and nothing wakes it again. The check gives it
 * ten seconds and fails with timeout's status 124. Blocking SIGCHLD
 * around the check and waiting with sigsuspend fixes it.
 */
#include <signal.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

#define ROUNDS 10

/* Writes a line to standard output; a failed write leaves it unsaid. */
static void say(const char *line, size_t len)
{
	if (write(1, line, len) != (ssize_t)len)
		return;
}

static volatile sig_atomic_t done;

static void on_child(int sig)
{
	(void)sig;
	done = 1;
}

int main(void)
{
	signal(SIGCHLD, on_child);
	for (int round = 0; round < ROUNDS; round++) {
		done = 0;
		pid_t worker = fork();
		if (worker == 0) {
			say("worker: done\n", 13);
			_exit(0);
		}

		/* The race: the signal can arrive between this check and pause. */
		if (!done) {
			say("parent: waiting\n", 16);
			pause();
		}
		waitpid(worker, NULL, 0);
	}
	printf("waiter: every worker waited for\n");
	return 0;
}
