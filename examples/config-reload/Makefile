# config-reload: a config rewritten in place while another process reads
# it, and a check that the reader never sees half of one.
CC ?= cc
CFLAGS ?= -O1 -g -Wall
PREFIX ?= /usr/local

all: config

check: config
	./config

install: config
	mkdir -p $(PREFIX)/bin
	cp config $(PREFIX)/bin/

clean:
	rm -f config app.conf

.PHONY: all check install clean
