/*
 * Shuts the pool down while workers are still busy, which is where
 * pool_shutdown's bug lives: each round waits for half the jobs, then
 * shuts down with the rest in flight. Whether a worker is inside the
 * unlocked window at that moment depends on the interleaving, so the test
 * runs a few rounds, as a stress test would.
 */
#include <stdio.h>
#include <unistd.h>

#include "../src/pool.h"

#define JOBS 32
#define ROUNDS 4

int main(void)
{
	for (int round = 0; round < ROUNDS; round++) {
		struct pool *p = pool_create();
		for (int i = 0; i < JOBS; i++)
			pool_submit(p, i);
		while (pool_completed(p) < JOBS / 2)
			usleep(100);
		pool_shutdown(p);
		printf("round %d: ok\n", round);
	}
	printf("test_pool_shutdown: ok\n");
	return 0;
}
