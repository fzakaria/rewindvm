# Tutorial: reproducible builds

<!-- let epoch = 1790985600 -->
<!-- let repo = github:fzakaria/rewindvm -->

A reproducible build produces the same bytes from the same inputs. Builds
can also depend on details outside a Nix derivation, such as random seeds
or directory order. Rewind lets you vary those details, compare the
outputs, and find the code responsible for a difference.

This tutorial fixes two builds so their outputs agree across those
variations. The [deterministic derivations tutorial](tutorial-derivations.md)
uses another approach: pin the VM's environment in the derivation and keep
the build code unchanged. Fixing the code reduces what the output depends
on; pinning the environment makes those dependencies explicit.

The examples are `tiles` and `plugins` from
[examples](https://github.com/fzakaria/rewindvm/tree/main/examples), two
small builds that each write one text file. Install Rewind as the
[Nix tutorial](tutorial-nix.md) does.

## Build it under eight seeds

`tiles` writes its pkg-config file with a Python script that keeps the
packages it links against in a set. Python iterates a set in an order drawn
from its hash seed. nixpkgs pins that seed with `PYTHONHASHSEED=0`, and
`tiles` unsets it.

A run's seed decides every random number the build draws, including Python's
hash seed. `check` builds `tiles` under eight seeds, each with the default
schedule only:

```console run name=seeds
$ rewind check --epoch {{epoch}} --seeds 8 --schedules 0 --all --no-narrow {{repo}}#tiles
```

<!-- capture seed0: seed +0 schedule +0: .* run (\w+) -->
<!-- capture seed1: seed +1 schedule +0: .* run (\w+) -->
<!-- capture hash0: seed +0 schedule +0: \S+ +\d+ steps +(\w+) -->
<!-- capture hash1: seed +1 schedule +0: \S+ +\d+ steps +(\w+) -->
<!-- assert: test {{hash0}} != {{hash1}} -->
<!-- set n_outputs: grep '^seed' {{out:seeds}} | awk '{print $8}' | sort -u | wc -l -->
<!-- assert: test {{n_outputs}} -ge 2 -->

The third column from the end is the start of the output's hash, and the
last is the run id. Eight builds made {{n_outputs}} different outputs, so
`tiles` is not reproducible.

## What differs

Compare seed 0's run with seed 1's:

```console run name=outputs elide
$ rewind diff --outputs {{seed0|short}} {{seed1|short}}
$ rewind diff --file /build/tiles/tiles.pc {{seed0|short}} {{seed1|short}}
```

<!-- set left_order: grep '^- Requires.private: ' {{out:outputs}} | cut -d: -f2- | tr -d ' ' -->
<!-- set right_order: grep '^+ Requires.private: ' {{out:outputs}} | cut -d: -f2- | tr -d ' ' -->
<!-- assert: for o in {{left_order}} {{right_order}}; do test "$(echo $o | tr , '\n' | sort | tr '\n' ' ')" = 'libpng pixman-1 zlib ' || exit 1; done && test {{left_order}} != {{right_order}} -->

`--outputs` lists the files the runs left behind whose bytes differ: the
installed `tiles.pc`, and the copy in the build directory it was installed
from. `--file` shows the lines: the same three packages in another order.

## Who wrote it

<!-- set pc: echo $(nix eval --raw {{repo}}#tiles.outPath 2>/dev/null)/lib/pkgconfig/tiles.pc -->

`rewind blame` takes two runs and a file, and names the program that wrote
the first byte that differs:

```console run name=blame elide
$ rewind blame {{seed0|short}} {{seed1|short}} {{pc}}
```

<!-- capture writer: differs at byte \d+\. (\S+) \(pid -->
<!-- capture pid: differs at byte \d+\. \S+ \(pid (\d+)\) -->
<!-- capture write_step: wrote that byte at step ([\d,]+) in run -->
<!-- assert: test {{writer}} = python3 -->
<!-- assert: grep -q '^  install (pid [0-9]*) copied it from /build/tiles/tiles.pc' {{out:blame}} -->
<!-- assert: grep -q '^Likely cause: something inside python3' {{out:blame}} -->

The first line is the answer: {{writer}}, pid {{pid}}, wrote that byte at
step {{write_step}}. The lines after it show how blame got there. `install`
only copied a file that already differed. Before that, pid {{pid}} opened
`/build/tiles/tiles.pc` while it was still `bash`, for the `>` redirect,
then became python3. python3 had the same command line and read the same
`gen-pc.py` in both runs, so the cause is inside it, and the seed is the one
input that changed.

Blame works from records the guest kernel keeps in every run: each write to
a file, and each file a program opens. It finds the write that set the byte
without replaying either run.

## In the app

Open the two runs side by side:

```console
$ rewind open {{seed0|short}} --compare {{seed1|short}}
```

The Compare tab's Files that differ link lists what `diff --outputs` does:

<!-- screenshot site/img/app-repro-files: {{home}}/runs/{{seed0}} --compare {{home}}/runs/{{seed1}} ;; click 1153 242 ;; click 1239 280 ;; wait 8 -->

![The app on seed 0's tiles run, compared with seed 1's, with the Files that differ tab listing both copies of tiles.pc](../site/img/app-repro-files.png)

A click on a file opens it from both runs. One under the other puts each
run's copy of a changed line together, this run's in amber and the other's
in blue. Go to the write moves the playhead to the step whose write set the
first byte that differs. For `/build/tiles/tiles.pc` that is step
{{write_step}}, with `python3 gen-pc.py` in the process tree:

<!-- screenshot site/img/app-repro-write: {{home}}/runs/{{seed0}} --compare {{home}}/runs/{{seed1}} ;; click 1153 242 ;; click 1239 280 ;; wait 8 ;; click 1000 387 ;; wait 10 ;; click 1263 415 ;; click 1123 346 ;; wait 10 -->

![The app at step {{write_step}}, where python3 writes tiles.pc, with the file from both runs one under the other and the Requires.private line in amber and blue](../site/img/app-repro-write.png)

## Fix it

<!-- run: git clone https://github.com/fzakaria/rewindvm -->
<!-- run: cd rewindvm -->

Sort the packages in `gen-pc.py` before writing them:

<!-- diff: tiles-fix.patch -->
<!-- run: patch -p1 < {{dir}}/tiles-fix.patch -->

```console run name=fixed show=-2:
$ rewind check --epoch {{epoch}} --seeds 8 --schedules 0 --all --no-narrow .#tiles
```

<!-- assert: grep -q '^0 of 7 other runs ended differently$' {{out:fixed}} -->

Every seed now builds the same `tiles.pc`.

## Directory order

`plugins` writes an index of the plugins it ships, in the order
`os.listdir` returns them. Under eight seeds it reproduces:

```console run name=plugins_seeds show=-2:
$ rewind check --epoch {{epoch}} --seeds 8 --schedules 0 --all --no-narrow .#plugins
```

<!-- capture stock: seed +0 schedule +0: .* run (\w+) -->
<!-- capture stock_steps: seed +0 schedule +0: \S+ +(\d+) steps -->
<!-- assert: grep -q '^0 of 7 other runs ended differently$' {{out:plugins_seeds}} -->

A directory lists in the order its filesystem keeps it. In the VM that order
is the same in every run: `/build` lists its newest file first. On a real
machine, ext4 lists a directory by a hash of each name, keyed by a seed
chosen when the disk was formatted, so two machines can list one directory
in two orders. The guest kernel does the same when its command line has
`rewind.dirents=N`: every directory lists in an order keyed by N.
`check --dirents 3` builds it again under keys 1 to 3 and compares each
build with the stock order, dirents 0:

```console run name=keys
$ rewind check --epoch {{epoch}} --seeds 1 --schedules 0 --all --no-narrow --dirents 3 .#plugins
```

<!-- capture key1: dirents +1 schedule +0: .* run (\w+) -->
<!-- capture key2: dirents +2 schedule +0: .* run (\w+) -->
<!-- capture keyhash0: dirents +0 schedule +0: \S+ +\d+ steps +(\w+) -->
<!-- capture keyhash1: dirents +1 schedule +0: \S+ +\d+ steps +(\w+) -->
<!-- capture keyhash2: dirents +2 schedule +0: \S+ +\d+ steps +(\w+) -->
<!-- capture key1_steps: dirents +1 schedule +0: \S+ +(\d+) steps -->
<!-- capture key2_steps: dirents +2 schedule +0: \S+ +(\d+) steps -->
<!-- assert: test {{keyhash1}} != {{keyhash2}} && test {{keyhash1}} != {{keyhash0}} && test {{keyhash2}} != {{keyhash0}} -->
<!-- assert: grep -q '^3 of 3 other runs ended differently$' {{out:keys}} -->

Each key builds another output. The index is the file that changed:

```console run name=index
$ rewind diff --file /build/plugins/plugins.txt {{key1|short}} {{key2|short}}
```

<!-- assert: rewind cat {{stock}} {{stock_steps}} /build/plugins/plugins.txt | tail -n +2 | tr '\n' ' ' | grep -qx 'sharpen rotate resize crop blur ' -->
<!-- assert: rewind cat {{key1}} {{key1_steps}} /build/plugins/plugins.txt | tail -n +2 | tr '\n' ' ' | grep -qx 'sharpen crop blur rotate resize ' -->
<!-- assert: rewind cat {{key2}} {{key2_steps}} /build/plugins/plugins.txt | tail -n +2 | tr '\n' ' ' | grep -qx 'sharpen resize crop rotate blur ' -->

Under each key the index names the same five plugins in another order.

The key is one of the run's inputs, so each order replays.

Sort the names in `gen-index.py`:

<!-- diff: plugins-fix.patch -->
<!-- run: patch -p1 < {{dir}}/plugins-fix.patch -->

```console run name=sorted show=-6:
$ rewind check --epoch {{epoch}} --seeds 1 --schedules 0 --all --no-narrow --dirents 3 .#plugins
```

<!-- assert: grep -q '^0 of 3 other runs ended differently$' {{out:sorted}} -->
<!-- capture sorted_hash: dirents +0 schedule +0: \S+ +\d+ steps +(\w+) -->

Every key now builds the same output, `{{sorted_hash}}`.

## What to read next

Real packages do this too: neatvnc 0.7.1's pkg-config file, which meson
writes, differs between seeds, and setuptools 80.9.0 in nixpkgs builds a
wheel with another version under another directory order.

- [Directory order](design.md#directory-order) in the design notes: how
  the `rewind.dirents` key works.
- [Read and write records](design.md#read-and-write-records) in the
  design notes: what blame reads.
- [The properties tutorial](tutorial-properties.md): seeds in tests.
