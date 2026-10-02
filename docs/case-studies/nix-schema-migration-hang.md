# Case study: a hang in Nix's store schema migration

In April 2026 a Nix master build could hang forever when several `nix`
commands opened a store at once, printing `SQLite database ... is busy` every
ten seconds. This is a known bug, reported as
[NixOS/nix#15693](https://github.com/NixOS/nix/issues/15693) and fixed in two
steps. Rewind VM reproduced it on the exact version in the report, pinned the
hang to one SQLite error code with gdb inside the VM, and showed why the first
fix did not stop the hang while the second did. None of the bug is a
discovery; the diagnosis of the hang is more specific than the one in the
issue.

## The software

`LocalStore::LocalStore` opens the store's SQLite database (in WAL mode) and
runs schema migrations. Since Nix 2.26 a migration is a named SQL script
recorded in a `SchemaMigrations` table, and on 2026-03-09 master added one
that every store runs, dropping the `IndexReferrer` index. At
[a94dee99e](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/src/libstore/local-store.cc#L566-L601),
the version the issue names, `upgradeDBSchema` reads the table, then runs each
missing migration in its own transaction:

```cpp
    auto doUpgrade = [&](const std::string & migrationName, const std::string & stmt) {
        if (schemaMigrations.contains(migrationName))
            return;

        debug("executing Nix database schema migration '%s'...", migrationName);

        SQLiteTxn txn(state.db);
        state.db.exec(stmt + fmt(";\ninsert into SchemaMigrations values('%s')", migrationName));
        txn.commit();

        schemaMigrations.insert(migrationName);
    };
```

Every process does this while holding the store's big lock in shared mode
only, so several can migrate at once. `SQLiteTxn` is a plain `begin;`, and
`SQLite::exec` retries its statement for as long as SQLite says the database
is busy
([sqlite.cc](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/src/libstore/sqlite.cc#L130-L136)):

```cpp
void SQLite::exec(const std::string & stmt)
{
    retrySQLite<void>([&]() {
        if (sqlite3_exec(db, stmt.c_str(), 0, 0, 0) != SQLITE_OK)
            SQLiteError::throw_(db, "executing SQLite statement '%s'", stmt);
    });
}
```

`retrySQLite` catches `SQLiteBusy`, sleeps up to 100 ms, prints a warning at
most every 10 seconds, and tries again, with no limit.

The test that shows it is
[tests/functional/ca/concurrent-builds.sh](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/tests/functional/ca/concurrent-builds.sh):
six `nix build` processes started at once on an empty store, with
content-addressed derivations on, so there are two migrations to run.

## Reproducing it

The derivation is Nix's own `nix-functional-tests` from its flake at
a94dee99e, which cache.nixos.org has, with the check phase cut down to
`meson test --no-rebuild concurrent-builds` and gdb added to the inputs so a
`rewind shell` can use it later. Under 256 perturbed schedules:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds-15693-gdb'
schedule   0: exited:0            33058 steps  d5ede538f628  run 1f3294d1cefa74d2
schedule   1: exited:0            41906 steps  d5ede538f628  run 2ca20d402deac28d
...
schedule 173: exited:1           400101 steps    run 6921fa19c24513c2
...
schedule 195: exited:1           546770 steps    run 425599d5bd3eff21
...
2 of 256 perturbed schedules ended differently

schedule 173 ends differently; narrowing the steps it perturbs
perturbing only steps 5032..23472 still ends differently

passing: run 1f3294d1cefa74d2
failing: run e2bf9ff720735a80

where nix build --no-link --file ./racy.nix first behaves differently:
  left       18023   220/220   execve("/nix/store/...-nix-2.35.0pre20260414_a94dee9/bin/nix", ["nix", "build", "--no-link", "--file", "./racy.nix"])
  ...
  right      19550   221/221   execve("/nix/store/...-nix-2.35.0pre20260414_a94dee9/bin/nix", ["nix", "build", "--no-link", "--file", "./racy.nix"])
  ...
```

Narrowing keeps most of the window here: the six processes race from the
moment they start, and the steps that matter are spread over all of them. The
same derivation without gdb in its inputs failed under 3 of 256 schedules. On
the host, the test passed 51 runs out of 51.

The failing run ends with meson's 300 second timeout, which the VM reaches in
seconds of wall time since a sleeping machine skips ahead to its next timer:

```console
$ rewind log 6921fa19 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      2 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.02s   killed by signal 15 SIGTERM
      3        insert into SchemaMigrations values('20251017-ca-derivations')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy

$ rewind events 6921fa19 | grep -E 'exit_group\(nix\)'
     25933   222/222   exit_group(nix) exited:1
     26209   224/224   exit_group(nix) exited:1
     26525   225/225   exit_group(nix) exited:1
     30396   238/238   exit_group(nix) killed:SIGTERM
     34885   223/223   exit_group(nix) exited:0
     37759   221/221   exit_group(nix) exited:0
    399953   220/220   exit_group(nix) exited:1
```

Both failure modes from the issue are in this one run. Processes 222, 224 and
225 lost the race to record the `20251017-ca-derivations` migration and exited
with the UNIQUE constraint error. Process 220 never got past opening the
store; it exited only after meson killed the test at step 399953. (Process 238
is `nix __build-remote`, the build hook; it ends with SIGTERM in passing runs
as well.) The events also show all six processes writing `var/nix/db/schema`,
so each of them took the new store branch of the constructor.

## Where the hung process is

At step 200000, in the middle of the hang, only 220 is left:

```console
$ rewind ps 6921fa19 --at 200000
     1 /init
    34   /nix/store/...-bash-5.3p3/bin/bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   187     /nix/store/...-python3-3.13.11/bin/python3.13 /nix/store/...-meson-1.9.1/bin/meson test --no-rebuild --print-errorlogs concurrent-builds
   188       /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh
   220         nix build --no-link --file ./racy.nix
   230           (thread)

$ rewind cat 6921fa19 200000 /proc/locks
1: POSIX  ADVISORY  READ 220 00:03:1812 124 124
2: POSIX  ADVISORY  READ 220 00:03:1812 128 128
3: POSIX  ADVISORY  READ 220 00:03:1809 1073741826 1073742335
4: FLOCK  ADVISORY  READ 220 00:03:1808 0 EOF
```

No other process holds a lock. 220 holds the big lock shared (inode 1808),
SQLite's shared lock on the database (1809), and in the WAL index (1812) the
read lock at offset 124, which is SQLite's read mark 1. That read lock means
220's connection is inside a read transaction, and nothing else is in its way.

`rewind shell` opens a shell in a throwaway fork of the run at a step, with
everything else stopped. gdb is in the closure, so it can attach to 220 there
and let it run until the next retry. The inspection leaves a SIGSTOP pending
on every process, which gdb has to be told to swallow:

```console
$ rewind shell 6921fa19 200000 --pid 220
rewind: a shell at step 200000 of 6921fa19c24513c2; exit it to leave
[rewind] /build/source/tests/functional/ca # cat /tmp/g.cmd
set pagination off
handle SIGSTOP nostop noprint nopass
set unwind-on-signal on
bt
break sqlite3_exec
continue
set $db = $rdi
printf "statement: %.200s\n", (char *) $rsi
finish
printf "sqlite3_exec returned %d\n", (int) $rax
printf "extended error code %d\n", (int) sqlite3_extended_errcode($db)
printf "errmsg %s\n", (char *) sqlite3_errmsg($db)
printf "autocommit %d\n", (int) sqlite3_get_autocommit($db)
detach
[rewind] /build/source/tests/functional/ca # gdb -p 220 -batch -x /tmp/g.cmd 2>&1 | grep -vE "^warning|auto-load|add-auto-load|line to your|To (enable|completely)|set auto-load|For more information|info .\(gdb\)|^$"
[New LWP 230]
...
#0  0x00007f2a6502a223 in clock_nanosleep@GLIBC_2.2.5 () from /nix/store/...-glibc-2.40-218/lib/libc.so.6
#1  0x00007f2a650368e7 in nanosleep () from /nix/store/...-glibc-2.40-218/lib/libc.so.6
#2  0x00007f2a65d81c1a in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#3  0x00007f2a65c2a8d5 in void nix::retrySQLite<void, nix::SQLite::exec(...)::{lambda()#1}>(...) ...
#4  0x00007f2a65d2837e in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&)::{lambda(...)#1}::operator()(...) ...
#5  0x00007f2a65d2a8a9 in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#6  0x00007f2a65d2b605 in nix::LocalStore::LocalStore(nix::ref<nix::LocalStoreConfig const>) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#7  0x00007f2a65d25046 in nix::LocalStoreConfig::openStore() const () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
...
Breakpoint 1 at 0x7f2a64241a68
Thread 1 "nix" hit Breakpoint 1, 0x00007f2a64241a68 in sqlite3_exec () from /nix/store/...-sqlite-3.50.4/lib/libsqlite3.so
statement: drop index if exists IndexReferrer;
insert into SchemaMigrations values('20260309-drop-redundant-indexreferrer')
...
sqlite3_exec returned 5
extended error code 517
errmsg database is locked
autocommit 0
[Inferior 1 (process 220) detached]
```

The command file was written with a heredoc earlier in the same session, cut
here. 220 is in `doUpgrade`, retrying the second migration. SQLite returns
`SQLITE_BUSY` (5) with the extended code 517, `SQLITE_BUSY_SNAPSHOT`, and
`autocommit 0` says the connection is inside the transaction that `SQLiteTxn`
began.

## Root cause

[SQLite's result code documentation](https://www.sqlite.org/rescode.html#busy_snapshot)
describes `SQLITE_BUSY_SNAPSHOT` as what a WAL connection gets when it tries
to turn a read transaction into a write transaction after another connection
has already written to the database. The connection's view of the database
is obsolete, and it stays obsolete until the transaction ends.

So the hang is this interleaving. 220 begins the migration's transaction and
reads (the `drop index if exists` reads the schema), which fixes its
snapshot. Another `nix` process commits a migration. 220's write is refused
with `SQLITE_BUSY_SNAPSHOT`. Nix maps every `SQLITE_BUSY` to `SQLiteBusy`, and
`SQLite::exec` retries the statement inside the same open transaction, on the
same stale snapshot, which SQLite refuses every time. The retry loop has no
limit: 28 warnings, ten seconds apart, until meson kills the test.

The UNIQUE failure is the other outcome of the same race: a process that
reads `SchemaMigrations` before another commits, and writes after it, with a
snapshot that is still current, runs the migration again and fails on the
insert.

## The two fixes, checked

[NixOS/nix#15694](https://github.com/NixOS/nix/pull/15694), merged on
2026-04-16, changed the insert to `insert or ignore`. That removes the UNIQUE
error but not the stale snapshot. The issue stayed open with the comment "Not
fixed, we are still observing this issue." The same check at the merge commit
c390460cd:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds-15694'
schedule   0: exited:0            34454 steps  d5ede538f628  run 2b4ba428c261fdee
...
schedule  22: exited:1           403388 steps    run 1b2bb898da558719
...
schedule  64: exited:1           404978 steps    run 1fa948d203fc4431
...
8 of 256 perturbed schedules ended differently

$ rewind log 1fa948d2 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      2 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.03s   killed by signal 15 SIGTERM
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy
```

None of the eight failing runs has a UNIQUE error, and all eight hang the same
way, each with 28 warnings.
[NixOS/nix#15967](https://github.com/NixOS/nix/pull/15967), merged on
2026-06-08, checks whether a migration is needed while holding the big lock
shared, and if one is, takes the lock exclusively to run it, then drops back
to shared and keeps holding it for the life of the store. With that, no other
Nix process can commit while a migration's transaction is open, so its
snapshot cannot go stale. Nix 2.35.2 has both fixes, and the same test from
nixpkgs' build of it passed every schedule:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds'
schedule   0: exited:0            34510 steps  d5ede538f628  run 29d7fba8cdf6e017
...
0 of 256 perturbed schedules ended differently
same result under all 257 schedules
```

`SQLite::exec` still retries inside whatever transaction it is called in. On
master the migrations are its only callers inside a `SQLiteTxn` in
`local-store.cc`, and they now run under the exclusive lock.

## Making it fail more often

Rewind's schedules now include stalls: at one exit in 128 the running task
sleeps for 10 µs to 1.28 ms when it next returns to user space. The rates
above were measured with them. With the previous version of Rewind, which only
reordered, the same derivation at a94dee99e failed under 3 of 256 schedules
and the first fix under 2 of 256, so stalls change the rates little here.

Earlier, to get more failing runs to study, an `LD_PRELOAD` library that
sleeps up to 5 ms at one call in 16 to `fcntl`, `flock`, `rename`, `unlink`,
`connect`, `pread64`, `pwrite64` and `fsync`, chosen by a hash of a seed, the
thread id and the call count, was preloaded into the test, under the previous
version of Rewind without stalls. It is not part of Rewind. With it:

| Nix                         | Schedules that hang |
| --------------------------- | ------------------- |
| a94dee99e, the reported bug | 15 of 65            |
| c390460cd, the first fix    | 16 of 65            |
| 2.35.2, both fixes          | 0 of 65             |

The shim's stalls are longer and fall on the calls around SQLite's locks,
which is where this race lives; Rewind's land on any exit.

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):
[nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd), with
everything needed to replay the failure on another AMD machine from Zen 2 on,
and [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd), the trace alone, which the
desktop app opens. `rewind import` and the app both take the URL, and
unpack the file as it downloads:

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay 6921fa19
$ rewind shell 6921fa19 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```

How they were made:

```console
$ rewind replay 6921fa19
identical: 3119 events over 400101 steps
$ rewind export 6921fa19 --replayable -o nix-schema-migration-hang-replayable.rwd
rewind: wrote nix-schema-migration-hang-replayable.rwd (433.5 MB)
$ rewind export 6921fa19 -o nix-schema-migration-hang.rwd
rewind: wrote nix-schema-migration-hang.rwd (39.6 KB)
```
