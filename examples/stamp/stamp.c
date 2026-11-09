/*
 * A program that says when it was built and which build made it, as
 * release builds like to. The Makefile reads a UUID from the kernel's
 * random pool and the time from the clock as it compiles, and passes both
 * in as BUILD_ID and BUILT_AT. No two builds on an ordinary machine make
 * the same program. In Rewind VM the pool's seed and the clock's start are
 * inputs of the run, so every build of one run's inputs does.
 */
#include <stdio.h>

int main(void)
{
	printf("build %s at %s\n", BUILD_ID, BUILT_AT);
	return 0;
}
