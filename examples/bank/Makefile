# bank: two tellers and one account, and a check that no deposit is lost.
CC ?= cc
CFLAGS ?= -O1 -g -Wall
LDLIBS = -lpthread
PREFIX ?= /usr/local

all: bank

check: bank
	./bank

install: bank
	mkdir -p $(PREFIX)/bin
	cp bank $(PREFIX)/bin/

clean:
	rm -f bank

.PHONY: all check install clean
