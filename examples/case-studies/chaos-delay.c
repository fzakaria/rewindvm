/* Stall the calling thread now and then at libc calls that touch files,
   locks and sockets, to emulate a process losing the CPU for a while on a
   loaded machine. A one-CPU deterministic VM otherwise keeps every process
   in near lockstep.

   Whether a call stalls, and for how long, is a hash of CHAOS_SEED, the
   thread id and how many wrapped calls that thread has made. It does not
   draw from a shared random stream, so a stall in one process does not
   change the stalls of the others.

   CHAOS_SEED:   varies the pattern of stalls (default 0).
   CHAOS_ONE_IN: stall at one call in this many (default 16).
   CHAOS_MAX_US: the longest stall, in microseconds (default 5000). */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/file.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static const unsigned DEFAULT_ONE_IN = 16;
static const unsigned DEFAULT_MAX_US = 5000;

static unsigned env_unsigned(const char *name, unsigned fallback)
{
	const char *s = getenv(name);
	if (s == NULL) {
		return fallback;
	}
	return (unsigned)strtoul(s, NULL, 10);
}

static uint64_t splitmix64(uint64_t x)
{
	x += 0x9e3779b97f4a7c15ULL;
	x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9ULL;
	x = (x ^ (x >> 27)) * 0x94d049bb133111ebULL;
	return x ^ (x >> 31);
}

/* Maybe sleep: one call in CHAOS_ONE_IN, for up to CHAOS_MAX_US. */
static void maybe_stall(void)
{
	static unsigned one_in;
	static unsigned max_us;
	static uint64_t seed;
	static __thread uint64_t calls;

	if (one_in == 0) {
		one_in = env_unsigned("CHAOS_ONE_IN", DEFAULT_ONE_IN);
		max_us = env_unsigned("CHAOS_MAX_US", DEFAULT_MAX_US);
		seed = env_unsigned("CHAOS_SEED", 0);
	}
	if (one_in == 0 || max_us == 0) {
		return;
	}

	uint64_t tid = (uint64_t)syscall(SYS_gettid);
	uint64_t r = splitmix64(seed ^ splitmix64(tid ^ splitmix64(calls++)));
	if ((r & 0xffffffff) % one_in != 0) {
		return;
	}

	unsigned us = (unsigned)(r >> 32) % max_us;
	struct timespec ts = { .tv_sec = us / 1000000, .tv_nsec = (long)(us % 1000000) * 1000 };
	nanosleep(&ts, NULL);
}

#define REAL(name) ((__typeof__(&name))dlsym(RTLD_NEXT, #name))

int fcntl(int fd, int cmd, ...)
{
	va_list ap;
	va_start(ap, cmd);
	void *arg = va_arg(ap, void *);
	va_end(ap);
	maybe_stall();
	return REAL(fcntl)(fd, cmd, arg);
}

int fcntl64(int fd, int cmd, ...)
{
	va_list ap;
	va_start(ap, cmd);
	void *arg = va_arg(ap, void *);
	va_end(ap);
	maybe_stall();
	return REAL(fcntl64)(fd, cmd, arg);
}

int flock(int fd, int op)
{
	maybe_stall();
	return REAL(flock)(fd, op);
}

int rename(const char *from, const char *to)
{
	maybe_stall();
	return REAL(rename)(from, to);
}

int unlink(const char *path)
{
	maybe_stall();
	return REAL(unlink)(path);
}

int connect(int fd, const struct sockaddr *addr, socklen_t len)
{
	maybe_stall();
	return REAL(connect)(fd, addr, len);
}

ssize_t pwrite64(int fd, const void *buf, size_t n, off_t off)
{
	maybe_stall();
	return REAL(pwrite64)(fd, buf, n, off);
}

ssize_t pread64(int fd, void *buf, size_t n, off_t off)
{
	maybe_stall();
	return REAL(pread64)(fd, buf, n, off);
}

int fsync(int fd)
{
	maybe_stall();
	return REAL(fsync)(fd);
}
