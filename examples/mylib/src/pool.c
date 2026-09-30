/*
 * A fixed-size thread pool.
 *
 * pool_shutdown has a bug that only some thread interleavings reach: it
 * frees the queue before joining the workers, and a worker that finished a
 * job checks whether the pool is stopping without the lock, then reports
 * the job and counts it through the queue. When the main thread runs
 * shutdown between that check and the count, the worker dereferences the
 * freed queue and the process dies with SIGSEGV. The window is a few
 * microseconds wide, so most runs never hit it, which is what makes the
 * test flaky.
 */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include "pool.h"

struct queue {
	int jobs[POOL_QUEUE_CAPACITY];
	int head;
	int count;
	int completed;
};

struct pool {
	pthread_mutex_t lock;
	pthread_cond_t ready;
	struct queue *queue;
	int stopping;
	pthread_t workers[POOL_WORKERS];
};

/* How long a job waits on its pretend I/O, in microseconds. */
#define JOB_IO_WAIT_US 40

/* The work itself: some arithmetic the compiler cannot fold away, and a
 * short wait, as a job that reads a file or a socket would have. */
static long run_job(int job)
{
	long acc = job;
	for (int i = 0; i < 20000; i++)
		acc = (acc * 6364136223846793005L + 1442695040888963407L) >> 1;
	usleep(JOB_IO_WAIT_US + (job % 3) * 10);
	return acc;
}

static void *worker(void *arg)
{
	struct pool *p = arg;

	for (;;) {
		/* Take the next job, or leave when the pool is stopping. */
		pthread_mutex_lock(&p->lock);
		while (!p->stopping && p->queue->count == 0)
			pthread_cond_wait(&p->ready, &p->lock);
		if (p->stopping) {
			pthread_mutex_unlock(&p->lock);
			return NULL;
		}
		struct queue *q = p->queue;
		int job = q->jobs[q->head];
		q->head = (q->head + 1) % POOL_QUEUE_CAPACITY;
		q->count--;
		printf("worker picked job %d\n", job);
		fflush(stdout);
		pthread_mutex_unlock(&p->lock);

		/* Run it, then report and count it unless the pool is
		 * stopping. The check and the count are not under the lock:
		 * the bug. */
		long result = run_job(job);
		if (!p->stopping) {
			printf("job %d done: %ld\n", job, result & 0xffff);
			fflush(stdout);
			p->queue->completed++;
		}
	}
}

struct pool *pool_create(void)
{
	struct pool *p = calloc(1, sizeof(*p));
	p->queue = calloc(1, sizeof(*p->queue));
	pthread_mutex_init(&p->lock, NULL);
	pthread_cond_init(&p->ready, NULL);
	for (int i = 0; i < POOL_WORKERS; i++)
		pthread_create(&p->workers[i], NULL, worker, p);
	return p;
}

int pool_submit(struct pool *p, int job)
{
	pthread_mutex_lock(&p->lock);
	if (p->queue->count == POOL_QUEUE_CAPACITY) {
		pthread_mutex_unlock(&p->lock);
		return -1;
	}
	int tail = (p->queue->head + p->queue->count) % POOL_QUEUE_CAPACITY;
	p->queue->jobs[tail] = job;
	p->queue->count++;
	pthread_cond_signal(&p->ready);
	pthread_mutex_unlock(&p->lock);
	return 0;
}

int pool_completed(struct pool *p)
{
	pthread_mutex_lock(&p->lock);
	int completed = p->queue->completed;
	pthread_mutex_unlock(&p->lock);
	return completed;
}

void pool_shutdown(struct pool *p)
{
	pthread_mutex_lock(&p->lock);
	p->stopping = 1;
	pthread_cond_broadcast(&p->ready);
	pthread_mutex_unlock(&p->lock);

	/* The bug: the queue goes before the workers are joined. */
	free(p->queue);
	p->queue = NULL;

	for (int i = 0; i < POOL_WORKERS; i++)
		pthread_join(p->workers[i], NULL);
	pthread_cond_destroy(&p->ready);
	pthread_mutex_destroy(&p->lock);
	free(p);
}
