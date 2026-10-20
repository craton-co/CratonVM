// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L6 -- the isolating bench rows of the
// virtual-thread compiled-entry switches (page
// i91-L6-virtual-thread-code-is-never-compiled): the same CPU-bound leaf
// (an int[1024] sum, no invoke / monitor op) run as 10 000 tasks on virtual
// threads and on a fixed pool of platform threads, each task calling it 32
// times. On HotSpot the two rates are close; here a virtual thread enters
// compiled code only for a callee its switches admit.
//
// Rows (each on virtual threads, then on platform threads):
//   static    -- `sum`, a static leaf through invokestatic
//                (CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY, wave 92, default on).
//   instance  -- `Summer.sum`, an instance leaf through invokevirtual
//                (CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF, wave 93, default off).
//   interface -- `Summing.sum`, the same leaf through invokeinterface
//                (CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF).
//   callers   -- `sumTwice`, a static body whose only invokes are two calls
//                of the static leaf, called 16 times
//                (CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS, wave 93, default off).
//   empty     -- the task only adds 1: the per-task virtual-thread
//                lifecycle (start, mount, run, end) with no leaf at all, so
//                the share of the virtual row the switches cannot touch.
//   sites     -- (wave 94) the task runs ~20 distinct cold-able sites once
//                (new, getfield / putfield, invokevirtual / invokeinterface,
//                instanceof / checkcast, JDK statics, a StringBuilder): the
//                isolating row of CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES, where
//                each new virtual thread starts with empty site caches unless
//                the switch hands it a dead one's (page
//                i93-L6-virtual-thread-start-pays-a-cold-invoke-cache).
//
// Census (wave 94): CRATONVM_DBG_VTHREAD_CENSUS=1 prints, at exit, the
// per-slice split (carrier wait, hand-off, mount, Java, unmount) and the
// per-thread register / teardown / registry / monitor-sweep times:
//   CRATONVM_DBG_VTHREAD_CENSUS=1 cratonvm L6W92VirtualThreadCpuBench 5 empty
//   CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES=1 cratonvm L6W92VirtualThreadCpuBench 5 sites
//   CRATONVM_VTHREAD_LEAN_REGISTRY=1 cratonvm L6W92VirtualThreadCpuBench 5 empty
//
// Timings go to stderr ("[bench] virtual ms=..." / "[bench] platform ms=..."
// for the static row, as in wave 92; "[bench] virtual-<row> ms=..." /
// "[bench] platform-<row> ms=..." for the others), best of `reps` (arg 0,
// default 5) after one warm-up rep per row; stdout is only the checksum
// lines below. Arg 1 picks one row (static, instance, interface, callers,
// empty; default all).
//
// A/B (interleave, take medians -- in-VM timings swing):
//   cratonvm L6W92VirtualThreadCpuBench 5
//   CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF=1 cratonvm L6W92VirtualThreadCpuBench 5 instance
//   CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF=1 cratonvm L6W92VirtualThreadCpuBench 5 interface
//   CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS=1 cratonvm L6W92VirtualThreadCpuBench 5 callers
//   CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY=0 cratonvm L6W92VirtualThreadCpuBench 5 static
//   (and with --compatible; java L6W92VirtualThreadCpuBench 5 for HotSpot)
//
// HOTSPOT_EXPECTED_BEGIN
// checksum virtual=167608320000 platform=167608320000
// checksum instance virtual=167608320000 platform=167608320000
// checksum interface virtual=167608320000 platform=167608320000
// checksum callers virtual=167608320000 platform=167608320000
// checksum empty virtual=10000 platform=10000
// checksum sites virtual=340000 platform=340000
// HOTSPOT_EXPECTED_END

import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

public class L6W92VirtualThreadCpuBench {
    static final int TASKS = 10_000;
    static final int CALLS = 32;

    static long sum(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    // A caller of leaves: its only invokes are two invokestatic of `sum`.
    static long sumTwice(int[] a) {
        return sum(a) + sum(a);
    }

    interface Summing {
        long sum(int[] a);
    }

    // The same leaf as an instance method (a getfield of its own is fine).
    static final class Summer implements Summing {
        final int start;

        Summer(int start) {
            this.start = start;
        }

        @Override
        public long sum(int[] a) {
            long s = 0;
            for (int i = start; i < a.length; i++) {
                s += a[i];
            }
            return s;
        }
    }

    // The `sites` row (wave 94): distinct sites a fresh thread meets cold.
    static final class Node {
        final int v;
        final Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
        }

        int value() {
            return v;
        }
    }

    interface Shape {
        int area();
    }

    static final class Sq implements Shape {
        final int s;

        Sq(int s) {
            this.s = s;
        }

        @Override
        public int area() {
            return s * s;
        }
    }

    static final class Rect implements Shape {
        final int w;
        final int h;

        Rect(int w, int h) {
            this.w = w;
            this.h = h;
        }

        @Override
        public int area() {
            return w * h;
        }
    }

    // 6 (list) + 9 (Sq) + 12 + 2 (max) + 2 ("29") + 3 ("x31") = 34 per task.
    static long sites(int[] data) {
        Node n = null;
        for (int i = 0; i < 4; i++) {
            n = new Node(i, n);
        }
        long s = 0;
        for (Node p = n; p != null; p = p.next) {
            s += p.value();
        }
        Object o = (s & 1) == 0 ? new Sq(3) : new Rect(2, 5);
        if (o instanceof Shape sh) {
            s += sh.area();
        }
        s += Integer.parseInt("12");
        s += Math.max(data[1], data[2]);
        s += String.valueOf(s).length();
        StringBuilder sb = new StringBuilder();
        sb.append('x').append(s);
        s += sb.length();
        return s;
    }

    static long task(String row, int[] data, Summer summer, Summing summing) {
        long s = 0;
        switch (row) {
            case "static" -> {
                for (int k = 0; k < CALLS; k++) {
                    s += sum(data);
                }
            }
            case "instance" -> {
                for (int k = 0; k < CALLS; k++) {
                    s += summer.sum(data);
                }
            }
            case "interface" -> {
                for (int k = 0; k < CALLS; k++) {
                    s += summing.sum(data);
                }
            }
            case "callers" -> {
                for (int k = 0; k < CALLS / 2; k++) {
                    s += sumTwice(data);
                }
            }
            case "sites" -> s = sites(data);
            default -> s = 1;
        }
        return s;
    }

    static long run(ExecutorService pool, String row, int[] data) throws InterruptedException {
        AtomicLong total = new AtomicLong();
        Summer summer = new Summer(0);
        Summing summing = summer;
        for (int t = 0; t < TASKS; t++) {
            pool.execute(() -> total.addAndGet(task(row, data, summer, summing)));
        }
        pool.shutdown();
        if (!pool.awaitTermination(10, TimeUnit.MINUTES)) {
            throw new IllegalStateException("timed out");
        }
        return total.get();
    }

    static long[] row(boolean virtual, String row, int reps, int[] data) throws InterruptedException {
        long best = Long.MAX_VALUE;
        long checksum = 0;
        for (int r = 0; r <= reps; r++) {
            ExecutorService pool = virtual
                    ? Executors.newVirtualThreadPerTaskExecutor()
                    : Executors.newFixedThreadPool(Runtime.getRuntime().availableProcessors());
            long t0 = System.nanoTime();
            checksum = run(pool, row, data);
            long ms = (System.nanoTime() - t0) / 1_000_000;
            if (r > 0) {
                best = Math.min(best, ms);
            }
        }
        return new long[] {best, checksum};
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        String only = args.length > 1 ? args[1] : "all";
        int[] data = new int[1024];
        for (int i = 0; i < data.length; i++) {
            data[i] = i;
        }
        for (String name : new String[] {"static", "instance", "interface", "callers", "empty", "sites"}) {
            if (!only.equals("all") && !only.equals(name)) {
                continue;
            }
            long[] v = row(true, name, reps, data);
            long[] p = row(false, name, reps, data);
            String suffix = name.equals("static") ? "" : "-" + name;
            System.err.println("[bench] virtual" + suffix + " ms=" + v[0]);
            System.err.println("[bench] platform" + suffix + " ms=" + p[0]);
            String label = name.equals("static") ? "" : name + " ";
            System.out.println("checksum " + label + "virtual=" + v[1] + " platform=" + p[1]);
        }
    }
}
