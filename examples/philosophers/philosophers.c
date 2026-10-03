/* The dining philosophers: five philosophers sit at a round table with a
 * fork between each pair, and a philosopher needs both of the forks beside
 * them to eat. Each philosopher is a thread and each fork a mutex. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

#define PHILOSOPHERS 5
#define MEALS 3

static pthread_mutex_t forks[PHILOSOPHERS];

static void *dine(void *arg)
{
	int id = (int)(intptr_t)arg;
	int left = id;
	int right = (id + 1) % PHILOSOPHERS;

	/* The fork on the left first, then the fork on the right. */
	int first = left;
	int second = right;

	/* Eat MEALS times, holding both forks for each meal. */
	for (int meal = 0; meal < MEALS; meal++) {
		pthread_mutex_lock(&forks[first]);
		printf("philosopher %d picks up fork %d\n", id, first);
		pthread_mutex_lock(&forks[second]);
		printf("philosopher %d picks up fork %d and eats\n", id, second);
		pthread_mutex_unlock(&forks[second]);
		pthread_mutex_unlock(&forks[first]);
	}
	return NULL;
}

int main(void)
{
	pthread_t philosophers[PHILOSOPHERS];

	/* Line buffer stdout, so every printf is one write and the log keeps
	 * the order the philosophers did things in. */
	setvbuf(stdout, NULL, _IOLBF, 0);

	/* Lay the table, seat everyone, and wait for them all to finish. */
	for (int i = 0; i < PHILOSOPHERS; i++) {
		pthread_mutex_init(&forks[i], NULL);
	}
	for (int i = 0; i < PHILOSOPHERS; i++) {
		pthread_create(&philosophers[i], NULL, dine, (void *)(intptr_t)i);
	}
	for (int i = 0; i < PHILOSOPHERS; i++) {
		pthread_join(philosophers[i], NULL);
	}
	printf("everyone ate\n");
	return 0;
}
