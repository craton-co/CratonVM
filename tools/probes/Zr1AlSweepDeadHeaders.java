// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z1/al (2026-10-12): the complement sweep's per-dead-object cost.
//
// The ZGC complement sweep (`gc/src/zgc/sweep.rs`, `sweep_bitmap_range`) reads
// no dead header, but by default it still WRITES one: a 16-byte zero store at
// every dead base. For small-object garbage (this probe's 40-byte nodes) those
// stores touch nearly every cache line of the reclaimed volume.
// `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS=1` (default OFF) leaves them unwritten
// and counts a word's dead objects with one popcount.
//
// Shape: a fixed live tree (the survivors whose headers the sweep must read
// anyway) and, per round, a burst of short-lived small nodes that are all
// dead by the time `System.gc()` runs a whole-heap cycle. Every round's
// checksum folds both, so an arm that reclaimed a live node or handed out a
// dirty span prints a different line from HotSpot.
//
// Output (stdout, identical on HotSpot and both CratonVM arms):
//   zr1al-sweep: rounds=<R> live=<L> burst=<B> checksum=<C>
// and, on its own line, the CPU time of the whole process (not compared):
//   zr1al-sweep-cpu-ms=<ms>
//
// Read the decision from stderr with `--verbose:gc`: `sweep_us=` on every
// `[GC] zgc-pause:` line, and `[GC] zgc-sweep-skip-dead-headers:
// cycle_skipped=` (the opt-in's engagement counter; it must be > 0 on the
// skip arm and the line absent on the default arm).
//
// Args: [rounds] [liveNodes] [burstNodes]   (defaults 40 200000 2000000)
import java.lang.management.ManagementFactory;

public final class Zr1AlSweepDeadHeaders {
    static final class Node {
        Node left;
        Node right;
        int value;

        Node(Node left, Node right, int value) {
            this.left = left;
            this.right = right;
            this.value = value;
        }
    }

    static Node build(int depth, int[] counter) {
        if (depth == 0) {
            return new Node(null, null, counter[0]++);
        }
        Node l = build(depth - 1, counter);
        Node r = build(depth - 1, counter);
        return new Node(l, r, counter[0]++);
    }

    static long sum(Node n) {
        long s = 0;
        java.util.ArrayDeque<Node> stack = new java.util.ArrayDeque<>();
        stack.push(n);
        while (!stack.isEmpty()) {
            Node x = stack.pop();
            s = s * 31 + x.value;
            if (x.left != null) stack.push(x.left);
            if (x.right != null) stack.push(x.right);
        }
        return s;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int liveNodes = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        int burst = args.length > 2 ? Integer.parseInt(args[2]) : 2_000_000;

        int depth = 1;
        while ((1 << (depth + 1)) - 1 < liveNodes) depth++;
        int[] counter = {0};
        Node live = build(depth, counter);
        int liveCount = counter[0];

        long cpu0 = processCpuNanos();
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            // The burst: a chain that dies wholesale at the end of the round.
            Node chain = null;
            long local = 0;
            for (int i = 0; i < burst; i++) {
                chain = new Node(chain, null, i ^ r);
                if ((i & 1023) == 0) {
                    // Drop the chain periodically so the dead are interleaved
                    // with nothing live and runs stay long.
                    local += chain.value;
                    chain = null;
                }
            }
            if (chain != null) local += chain.value;
            chain = null;
            System.gc();
            checksum = checksum * 1_000_003L + local + sum(live);
        }
        long cpuMs = (processCpuNanos() - cpu0) / 1_000_000L;
        System.out.println("zr1al-sweep: rounds=" + rounds + " live=" + liveCount
                + " burst=" + burst + " checksum=" + checksum);
        System.out.println("zr1al-sweep-cpu-ms=" + cpuMs);
    }

    static long processCpuNanos() {
        java.lang.management.OperatingSystemMXBean os =
                ManagementFactory.getOperatingSystemMXBean();
        if (os instanceof com.sun.management.OperatingSystemMXBean) {
            return ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime();
        }
        return ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
    }
}
