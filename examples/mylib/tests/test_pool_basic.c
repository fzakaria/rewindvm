/* Submits jobs and waits for them all to finish before shutting down. */
#include <stdio.h>
#include <unistd.h>

#include "../src/pool.h"

#define JOBS 8

int main(void)
{
	struct pool *p = pool_create();
	for (int i = 0; i < JOBS; i++)
		pool_submit(p, i);
	while (pool_completed(p) < JOBS)
		usleep(1000);
	pool_shutdown(p);
	printf("test_pool_basic: ok\n");
	return 0;
}
