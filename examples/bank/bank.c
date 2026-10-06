/*
 * Two tellers deposit into one account. A deposit reads the balance,
 * writes a line to the ledger, then stores the new balance, all without a
 * lock, so a teller that runs between another's read and store has its
 * deposit written over. The check has both tellers deposit and fails when
 * the balance comes up short.
 */
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>

#define DEPOSITS 20
#define AMOUNT 10

static long balance;

/* The race: the balance read here is stale by the time it is stored. */
static void deposit(int teller, long amount)
{
	long seen = balance;
	char line[64];
	int n = snprintf(line, sizeof line, "teller %d: %ld + %ld\n", teller, seen, amount);

	if (write(1, line, n) != n)
		return;
	balance = seen + amount;
}

static void *teller(void *arg)
{
	int id = (int)(long)arg;

	for (int i = 0; i < DEPOSITS; i++)
		deposit(id, AMOUNT);
	return NULL;
}

int main(void)
{
	pthread_t first, second;
	long expected = 2L * DEPOSITS * AMOUNT;

	pthread_create(&first, NULL, teller, (void *)1L);
	pthread_create(&second, NULL, teller, (void *)2L);
	pthread_join(first, NULL);
	pthread_join(second, NULL);

	printf("balance %ld, expected %ld\n", balance, expected);
	return balance == expected ? 0 : 1;
}
