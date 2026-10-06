# waiter: a parent that waits for its workers with pause, and a check that
# it always wakes.
CC ?= cc
CFLAGS ?= -O1 -g -Wall
PREFIX ?= /usr/local

# How long the rounds may take before the check calls it a hang. A lost
# wakeup never ends on its own, so timeout ends it with status 124.
WAIT_SECONDS = 10

all: waiter

check: waiter
	timeout $(WAIT_SECONDS) ./waiter

install: waiter
	mkdir -p $(PREFIX)/bin
	cp waiter $(PREFIX)/bin/

clean:
	rm -f waiter

.PHONY: all check install clean
