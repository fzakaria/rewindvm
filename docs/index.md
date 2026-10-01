# Rewind VM documentation

1. [Tutorial: a flaky Nix build](tutorial-nix.md): install, find the
   interleaving that breaks a derivation's tests, look at it step by step,
   fork it, and check the fix.
2. [Tutorial: a flaky test in a container](tutorial-container.md): the same
   with a Docker image and no Nix.
3. [Time inside the VM](pmu.md): exit time and counter time, and the one
   setting AMD machines need.
4. [Design](design.md): how the machine is made deterministic, what a run is
   on disk, keyframes and the page store, exploring interleavings, limits,
   and the product.
