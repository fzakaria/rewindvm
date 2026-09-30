# mylib: a thread pool and its tests.
CC ?= cc
CFLAGS ?= -O1 -g -Wall
LDLIBS = -lpthread
PREFIX ?= /usr/local

TESTS = tests/test_pool_basic tests/test_pool_shutdown

all: libmylib.a $(TESTS)

libmylib.a: src/pool.o
	$(AR) rcs $@ $^

tests/%: tests/%.c libmylib.a
	$(CC) $(CFLAGS) -o $@ $< libmylib.a $(LDLIBS)

check: $(TESTS)
	@for t in $(TESTS); do echo "running $$t"; ./$$t || exit 1; done

install: libmylib.a
	mkdir -p $(PREFIX)/lib $(PREFIX)/include
	cp libmylib.a $(PREFIX)/lib/
	cp src/pool.h $(PREFIX)/include/

clean:
	rm -f src/*.o libmylib.a $(TESTS)

.PHONY: all check install clean
