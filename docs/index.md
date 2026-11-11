# Rewind VM documentation

1. [Tutorial: a flaky Nix build](tutorial-nix.md): install, find the
   interleaving that breaks a derivation's tests, look at it step by step,
   fork it, and check the fix.
2. [Tutorial: a flaky test in a container](tutorial-container.md): the same
   with a Docker image and no Nix.
3. [Tutorial: property-based testing](tutorial-properties.md): properties,
   assertions from inside the program with the Rewind SDK, and seeds.
4. [Tutorial: schedule points](tutorial-points.md): one line that puts a
   thread switch where no system call is.
5. [Tutorial: reproducible builds](tutorial-repro.md): build under several
   seeds and directory orders, see what differs, and find the program that
   wrote it.
6. [Tutorial: deterministic derivations](tutorial-derivations.md): have Nix
   pin a build's environment with `inRewind` and check that a rebuild
   produces the same bytes.
7. [Tutorial: advanced features](tutorial-advanced.md): the rest of the tools,
   recipe by recipe.
8. [Time inside the VM](pmu.md): exit time and counter time, and the one
   setting AMD machines need.
9. [Design](design.md): how the machine is made deterministic, what a run is
   on disk, keyframes and the page store, exploring interleavings, limits,
   and the product.

[Nix build reference](nix-builds.md): machine settings, runtime identity,
the external builder, and exporting a recorded build's files.

[Notes](notes/README.md): dated audits, reviews and design investigations.
