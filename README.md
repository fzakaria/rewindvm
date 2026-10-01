<p align="center">
  <img src="site/img/mark.svg" width="64" height="64" alt="" />
</p>

<h1 align="center">Rewind VM</h1>

<p align="center">
  <b>Deterministic Linux VMs you can scrub, rewind and fork.</b><br />
  Catch a flaky build or test once, then replay it exactly, step through it, and branch it.
</p>

<p align="center">
  <a href="https://rewindvm.dev">Website</a> ·
  <a href="docs/tutorial-nix.md">Nix tutorial</a> ·
  <a href="docs/tutorial-container.md">Container tutorial</a> ·
  <a href="docs/design.md">Design</a> ·
  <a href="https://github.com/fzakaria/rewindvm/releases">Releases</a>
</p>

<p align="center">
  <a href="https://github.com/fzakaria/rewindvm/actions/workflows/ci.yml"><img src="https://github.com/fzakaria/rewindvm/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
</p>

<p align="center">
  <img src="site/img/app-failure.png" alt="The Rewind desktop app on a failing Nix build: the timeline of build phases, the build log up to the playhead, the processes alive at that step, the SIGSEGV that ended the test, and a card saying where this run parted from a passing run of the same build." />
</p>

Rewind VM runs a Nix build, a test suite or any Linux command inside a KVM
virtual machine whose every run is a function of its inputs. The same inputs
give the same run, at the same steps, every time. A failure you saw once is a
failure you keep: replay it, scrub through it step by step, read any file as it
was at any step, and fork it with a different thread interleaving from any
point.

```console
$ rewind check github:fzakaria/rewindvm#mylib
schedule   0: exited:0             6173 steps  aa30ea54dc47  run 1c9df920ccb1e3f3
schedule   1: exited:0             6652 steps  aa30ea54dc47  run a3529e91ea9b4da2
schedule   2: exited:0             6681 steps  aa30ea54dc47  run aabf85180a60b1a9
schedule   3: exited:2             5098 steps    run 90dc4491b5162f37
...

schedule 3 ends differently; narrowing the steps it perturbs
perturbing only steps 3095..5045 still ends differently

passing: run 1c9df920ccb1e3f3
failing: run b626a706bc163995

where ./tests/test_pool_shutdown first behaves differently:
  ...
  left        4136   165/166   write(1, "job 0 done: 12727\n")
  left        4137   165/166   write(1, "worker picked job 2\n")
  right       4222   165/167   write(1, "job 1 done: 35269\n")
  right       4223   165/167   write(1, "worker picked job 2\n")

$ rewind events b626a706 | grep SIGSEGV
      4431   165/166   SIGSEGV code=1 addr=0x108

$ rewind replay b626a706
identical: 1591 events over 4470 steps
```

## Install

On x86_64 Linux with KVM:

```console
$ curl -fsSL https://rewindvm.dev/install | sh
```

With Nix, run it straight from the flake, or install it into your profile:

```console
$ nix run github:fzakaria/rewindvm -- pmu status
$ nix run github:fzakaria/rewindvm#app
$ nix profile install github:fzakaria/rewindvm github:fzakaria/rewindvm#app
```

On NixOS, add the flake as an input and turn on its module:

```nix
inputs.rewind.url = "github:fzakaria/rewindvm";

# in your configuration, with inputs.rewind.nixosModules.default imported
programs.rewind.enable = true;
programs.rewind.app.enable = true;
# AMD only: make the branch counter exact at every boot
programs.rewind.amdBranchCounterWorkaround = true;
```

Or take the tarballs from the
[latest release](https://github.com/fzakaria/rewindvm/releases/latest)
yourself:

```console
# the rewind command, with the VM's kernel
$ curl -L https://github.com/fzakaria/rewindvm/releases/latest/download/rewind-x86_64-linux.tar.gz | tar xz
# the desktop app
$ curl -L https://github.com/fzakaria/rewindvm/releases/latest/download/rewind-app-x86_64-linux.tar.gz | tar xz
```

`/dev/kvm` must be readable and writable by you. On AMD Ryzen and EPYC, run
`sudo rewind pmu enable` once after each boot so the VM's clock can follow the
work done inside it; [Time inside the VM](docs/pmu.md) explains why.

## Use it

```console
# build a derivation in the VM; the output is checked against the host's
$ rewind nix nixpkgs#hello

# run the derivation under many thread schedules and show where a failure parts ways
$ rewind check github:fzakaria/rewindvm#mylib

# any command in a root filesystem: a directory, an erofs image, or a docker export
$ rewind run --root mylib.tar --cwd /src -- make check

# list runs and look inside one
$ rewind ls
$ rewind log <run> --steps
$ rewind ps <run> --at <step>
$ rewind cat <run> <step> /build/env-vars

# a shell inside the VM at a step, or gdb on it, in a throwaway fork
$ rewind shell <run> <step> --pid <pid>
$ rewind gdb <run> <step>

# branch a run at a step under another schedule, or replay it exactly
$ rewind fork <run> <step> --schedule 2
$ rewind replay <run>

# share a run as one file
$ rewind export <run> --replayable
$ rewind import <file>.rwd
```

## The desktop app

The app scrubs a recorded run: drag the playhead over the timeline of phases,
and the build log, the process tree and the files follow it. It jumps to the
failure, or to the first point where the run parts from a passing one and says
in words what each did next. Click a file to read it as it was at the
playhead. Fork from here branches the run under a new schedule.

<p align="center">
  <img src="docs/img/app-file-viewer.png" alt="The Rewind desktop app with a file open at the playhead: /build/env-vars as of step 4,392, next to the build log and the process tree." />
</p>

```console
$ nix run github:fzakaria/rewindvm#app -- ~/.local/share/rewind/runs/<run>
```

It opens `.rwd` exports too, and comes with an example run and a short tour.
See [pricing](https://rewindvm.dev/#pricing) for licenses.

## How it works

The VM has one vCPU on stock KVM, so code in it runs on the real CPU. Its
Linux kernel carries a small Rewind platform: interrupts arrive only when the
VM hands control to Rewind, the clock moves only then, and the timestamp
counter and hardware RNG are hidden. Each of those handoffs is a step, and the
sequence of steps is the same on every run. Inputs, a Nix closure or a root
filesystem, become a read-only erofs image mapped into the VM's memory.
Keyframes of the VM's memory go into a content-addressed page store, so seeking
to any step restores the nearest keyframe and runs forward.

A GNU hello build from nixpkgs runs at close to native speed in the VM, and its
output is bit for bit the one Nix builds on the host.
[Design](docs/design.md) has the details, the limits, and what comes next.

## Develop

```console
$ nix develop                  # the toolchain, with REWIND_KERNEL and REWIND_INITRD set
$ cargo test --workspace
$ cargo run --release -p rewind -- nix nixpkgs#hello
$ nix flake check              # prose, the NixOS module, and a determinism check that needs /dev/kvm
$ nix fmt                      # Nix, Rust, Python, HTML and Markdown
$ nix run .#serve              # the website on a local port
```

- `guest/linux/`: the kernel patch and config fragment for the VM.
- `crates/rewind-vmm/`: the virtual machine monitor: KVM, boot, steps, time,
  keyframes.
- `crates/rewind-init/`: PID 1 inside the VM, a workspace of its own.
- `crates/rewind-trace/`: reading and querying run traces.
- `crates/rewind-store/`: the content-addressed page store.
- `crates/rewind-core/`: input images, Nix derivations, runs, seeking.
- `crates/rewind/`: the `rewind` command.
- `crates/rewind-app/`: the desktop app, a workspace of its own.
- `examples/mylib/`: the flaky thread pool the tutorials use.
- `nix/`: one file per derivation; `flake.nix` only wires them together.
- `site/`: rewindvm.dev.
- `docs/`: tutorials, the time reference and the design.
- `tools/`: the prose check and the local site server.

Pushing a `v*` tag that matches the version in `Cargo.toml` publishes a
release with both tarballs.

## License

| Path                 | License                                                                                            |
| -------------------- | -------------------------------------------------------------------------------------------------- |
| `crates/rewind-app/` | Proprietary to Lunch Time Surf LLC, source available, see [its LICENSE](crates/rewind-app/LICENSE) |
| `guest/linux/`       | GPL-2.0-only, as Linux is, see [guest/linux/LICENSE](guest/linux/LICENSE)                          |
| everything else      | MIT, see [LICENSE](LICENSE)                                                                        |

The engine and the command are open source. The desktop app's source is here
to read, but copying, modifying or redistributing it needs permission.
