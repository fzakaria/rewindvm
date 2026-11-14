#!/usr/bin/env python3
"""Tests for the benchmark's guards against numbers that cannot be compared.

python3 -m unittest discover -s tools -p 'test_*.py'
"""

import unittest

import bench

NOMINAL_MHZ = 2450
BOOSTED_MHZ = 3530


def record(label, host, clock_mhz, measured):
    # A history record with only the fields the clock guard reads.
    machine = {"host": host}
    if clock_mhz is not None:
        machine["clock_mhz"] = clock_mhz
    return {"label": label, "machine": machine, "measured": measured}


class ClockGuard(unittest.TestCase):
    # A run checks its measured clock against the newest record from the
    # same host that says its clock, so a machine that starts boosting, or
    # stops, cannot add records the page would draw beside the old ones.
    def test_a_boosting_machine_is_caught(self):
        # leviathan ran at its nominal clock for every release up to
        # 0.16.0 and came back from a reboot boosting 44% higher.
        records = [record("v0.15.0", "leviathan", NOMINAL_MHZ, "2026-11-10")]
        found = bench.clock_mismatch(records, "leviathan", BOOSTED_MHZ)
        self.assertEqual(found["label"], "v0.15.0")

    def test_a_clock_within_the_tolerance_is_the_same_clock(self):
        # A busy core reads a little under its nominal clock from one
        # moment to the next; that is the same machine.
        records = [record("v0.15.0", "leviathan", 2451, "2026-11-10")]
        self.assertIsNone(bench.clock_mismatch(records, "leviathan", 2420))

    def test_the_newest_record_with_a_clock_is_the_one_compared(self):
        # An older record at another clock does not matter once a newer
        # one at this clock was accepted.
        records = [
            record("v0.16.0", "leviathan", BOOSTED_MHZ, "2026-11-20"),
            record("v0.15.0", "leviathan", NOMINAL_MHZ, "2026-11-10"),
        ]
        self.assertIsNone(bench.clock_mismatch(records, "leviathan", BOOSTED_MHZ))

    def test_records_without_a_clock_or_from_elsewhere_are_not_compared(self):
        # Records from before the clock was measured, and records from
        # another host, say nothing about this host's clock.
        records = [
            record("v0.14.0", "leviathan", None, "2026-11-09"),
            record("v0.15.0", "nyx", NOMINAL_MHZ, "2026-11-10"),
        ]
        self.assertIsNone(bench.clock_mismatch(records, "leviathan", BOOSTED_MHZ))

    def test_a_host_without_a_clock_reading_is_not_compared(self):
        # A machine without cpufreq, such as a VM, cannot say its clock.
        records = [record("v0.15.0", "leviathan", NOMINAL_MHZ, "2026-11-10")]
        self.assertIsNone(bench.clock_mismatch(records, "leviathan", None))


class NodeChoice(unittest.TestCase):
    # --nodes keeps the benchmark on the NUMA nodes whose clock was pinned,
    # out of every node the machine has.
    NODES = [(0, "0-15,128-143"), (6, "96-111,224-239"), (7, "112-127,240-255")]

    def test_only_the_named_nodes_are_kept(self):
        # The nodes come back in the machine's order with their CPU lists.
        kept = bench.pick_nodes(self.NODES, "7,6")
        self.assertEqual(kept, [(6, "96-111,224-239"), (7, "112-127,240-255")])

    def test_no_choice_keeps_every_node(self):
        # Without --nodes the idlest of all the nodes is picked, as before.
        self.assertEqual(bench.pick_nodes(self.NODES, None), self.NODES)

    def test_a_node_the_machine_lacks_is_refused(self):
        # A typo must not quietly measure on fewer nodes than asked for.
        with self.assertRaises(SystemExit):
            bench.pick_nodes(self.NODES, "6,9")


if __name__ == "__main__":
    unittest.main()
