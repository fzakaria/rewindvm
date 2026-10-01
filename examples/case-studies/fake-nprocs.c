/* Make a program in a one-CPU VM size its thread pools as if it had
   FAKE_NPROCS CPUs (default 4). Overrides glibc's get_nprocs(),
   get_nprocs_conf() and sysconf(_SC_NPROCESSORS_*), which is what
   std::thread::hardware_concurrency() and most thread pools read. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdlib.h>
#include <unistd.h>

static const int DEFAULT_NPROCS = 4;

static int fake_nprocs(void)
{
	const char *s = getenv("FAKE_NPROCS");
	if (s == NULL) {
		return DEFAULT_NPROCS;
	}
	return atoi(s);
}

int get_nprocs(void)
{
	return fake_nprocs();
}

int get_nprocs_conf(void)
{
	return fake_nprocs();
}

long sysconf(int name)
{
	static long (*real_sysconf)(int);

	if (name == _SC_NPROCESSORS_ONLN || name == _SC_NPROCESSORS_CONF) {
		return fake_nprocs();
	}
	if (real_sysconf == NULL) {
		real_sysconf = (long (*)(int))dlsym(RTLD_NEXT, "sysconf");
	}
	return real_sysconf(name);
}
