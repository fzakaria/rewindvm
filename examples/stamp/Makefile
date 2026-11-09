# stamp: a program stamped with the id and the time of its build.
CC ?= cc
CFLAGS ?= -O1 -Wall
PREFIX ?= /usr/local

# The stamp: a UUID from the kernel's random pool, and the clock.
BUILD_ID := $(shell cat /proc/sys/kernel/random/uuid)
BUILT_AT := $(shell date -u +%FT%TZ)

all: stamp

stamp: stamp.c
	$(CC) $(CFLAGS) -DBUILD_ID='"$(BUILD_ID)"' -DBUILT_AT='"$(BUILT_AT)"' -o $@ $<

install: stamp
	mkdir -p $(PREFIX)/bin
	cp stamp $(PREFIX)/bin/

clean:
	rm -f stamp

.PHONY: all install clean
