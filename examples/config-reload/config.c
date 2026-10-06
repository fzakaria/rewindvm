/*
 * A writer rewrites a config file in place, truncating it and then
 * writing its two lines one at a time, while a reader in another process
 * rereads it. A reader that opens the file between the truncate and the
 * last write sees a config with a line missing. The check runs both and
 * fails when the reader ever saw one. Writing a temporary file and
 * renaming it over the config fixes it.
 */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define PATH "app.conf"
#define WRITES 20
#define READS 40

static const char *const lines[] = {"name=rewind\n", "mode=fast\n"};

/* The race: between the truncate and the last write the file is short. */
static void write_config(void)
{
	int fd = open(PATH, O_WRONLY | O_CREAT | O_TRUNC, 0644);

	for (size_t i = 0; i < sizeof lines / sizeof lines[0]; i++) {
		size_t len = strlen(lines[i]);

		if (write(fd, lines[i], len) != (ssize_t)len)
			break;
	}
	close(fd);
}

/* Reads the config again and again; 1 when it was ever short. */
static int read_configs(void)
{
	char buf[128];

	for (int i = 0; i < READS; i++) {
		int fd = open(PATH, O_RDONLY);
		ssize_t n = read(fd, buf, sizeof buf - 1);

		close(fd);
		buf[n < 0 ? 0 : n] = '\0';
		if (!strstr(buf, "name=") || !strstr(buf, "mode=")) {
			printf("reader: read %zd bytes, a config with a line missing\n", n);
			return 1;
		}
	}
	printf("reader: every config whole\n");
	return 0;
}

int main(void)
{
	int status;

	write_config();
	pid_t reader = fork();
	if (reader == 0)
		_exit(read_configs());
	for (int i = 0; i < WRITES; i++)
		write_config();
	waitpid(reader, &status, 0);
	return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
