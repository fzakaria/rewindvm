/* Passes a token around a ring of threads, each waiting on a condition
 * variable for its turn, and prints how many passes were made. Every pass
 * wakes one thread and puts another to sleep, so the run is almost all
 * context switches: the benchmark's measure of what a thread switch costs
 * under Rewind.
 *
 *   pingpong <threads> <passes> */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t turn = PTHREAD_COND_INITIALIZER;
static long holder;
static long passes;
static long threads;
static long limit;

static void *player(void *arg)
{
  long me = (long)arg;

  pthread_mutex_lock(&lock);
  for (;;) {
    /* Wait for the token, or for the game to end. */
    while (holder != me && passes < limit) {
      pthread_cond_wait(&turn, &lock);
    }
    if (passes >= limit) {
      break;
    }

    /* Hand the token to the next thread in the ring. */
    holder = (me + 1) % threads;
    passes++;
    pthread_cond_broadcast(&turn);
  }
  pthread_mutex_unlock(&lock);
  return NULL;
}

int main(int argc, char **argv)
{
  if (argc != 3) {
    fprintf(stderr, "usage: pingpong <threads> <passes>\n");
    return 2;
  }
  threads = atol(argv[1]);
  limit = atol(argv[2]);

  pthread_t *ids = calloc(threads, sizeof(*ids));
  for (long i = 0; i < threads; i++) {
    pthread_create(&ids[i], NULL, player, (void *)i);
  }
  for (long i = 0; i < threads; i++) {
    pthread_join(ids[i], NULL);
  }

  printf("%ld passes\n", passes);
  return 0;
}
