# philosophers: the dining philosophers, and a check that dinner ends.
CC ?= cc
CFLAGS ?= -O1 -g -Wall
LDLIBS = -lpthread
PREFIX ?= /usr/local

# How long dinner may take before the check calls it a deadlock. A
# deadlock never exits on its own, so timeout ends it with status 124.
DINNER_SECONDS = 10

all: philosophers

check: philosophers
	timeout $(DINNER_SECONDS) ./philosophers

install: philosophers
	mkdir -p $(PREFIX)/bin
	cp philosophers $(PREFIX)/bin/

clean:
	rm -f philosophers

.PHONY: all check install clean
