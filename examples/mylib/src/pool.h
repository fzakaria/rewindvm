/* A fixed-size thread pool: submit integer jobs, then shut it down. */
#ifndef MYLIB_POOL_H
#define MYLIB_POOL_H

struct pool;

/* The number of worker threads every pool starts. */
#define POOL_WORKERS 2

/* How many jobs can wait in the queue at once. */
#define POOL_QUEUE_CAPACITY 64

struct pool *pool_create(void);
int pool_submit(struct pool *p, int job);
int pool_completed(struct pool *p);
void pool_shutdown(struct pool *p);

#endif
