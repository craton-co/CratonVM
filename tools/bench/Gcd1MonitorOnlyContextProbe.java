// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcd d1/mon (2026-09-27): a compiled method whose ONLY context-taking
 * operations are its monitors. Page:
 * {@code docs/internal/gc/gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer-FIXED-20260928.md}.
 *
 * <p>{@code bump} and {@code bumpNested} touch no field, call nothing and
 * allocate nothing inside their {@code synchronized} blocks: array element
 * arithmetic on a parameter only. Before the fix the optimizing tier reserved
 * no VM-context slot for such a body ({@code ir_lower::scan_frame_needs} did
 * not list the monitor nodes), so its monitor stub loaded {@code [rbp - 0]} --
 * the saved caller RBP -- as the helper's {@code SharedVm}. The inline thin
 * lock never reads the context, so the wild pointer is dereferenced only when
 * a monitor op falls to the helper: under contention (the four threads here)
 * or always with {@code CRATONVM_MONITOR_FASTPATH=0}. It faulted in
 * {@code VmHeap::flush_thread_satb} ({@code addr=0x251}/{@code 0x247}) or in
 * {@code LockSlots::lease}; with the helper screen and no lowering fix it
 * aborts with a {@code [cratonvm] FATAL jit_monitor_enter: vm_ptr=...} line.
 *
 * <p>Deterministic stdout (HotSpot prints the same):
 * <pre>
 *   monitor-only-sum 4000000
 *   monitor-only-nested-sum 8000000
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -cp tools/bench Gcd1MonitorOnlyContextProbe
 *   cratonvm --compatible --java-home "$JDK" -XX:+UseGenerationalGC -cp tools/bench Gcd1MonitorOnlyContextProbe
 *   CRATONVM_MONITOR_FASTPATH=0 cratonvm --compatible --java-home "$JDK" -XX:+UseGenerationalGC -cp tools/bench Gcd1MonitorOnlyContextProbe
 * </pre>
 */
public class Gcd1MonitorOnlyContextProbe {
    static final int THREADS = 4;
    static final int ROUNDS = 1_000_000;

    /** A monitor around primitive array arithmetic: nothing else in the body needs the VM. */
    static void bump(Object lock, int[] cells, int i) {
        synchronized (lock) {
            cells[i & 15] += 1;
        }
    }

    /** Two nested monitors, always taken in the same order (no deadlock). */
    static void bumpNested(Object outer, Object inner, int[] cells, int i) {
        synchronized (outer) {
            synchronized (inner) {
                cells[i & 15] += 2;
            }
        }
    }

    static long sum(int[] cells) {
        long s = 0;
        for (int c : cells) {
            s += c;
        }
        return s;
    }

    public static void main(String[] args) throws InterruptedException {
        final Object lock = new Object();
        final Object inner = new Object();
        final int[] cells = new int[16];
        final int[] nestedCells = new int[16];
        Thread[] workers = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int seed = t;
            workers[t] = new Thread(() -> {
                for (int i = 0; i < ROUNDS; i++) {
                    bump(lock, cells, i + seed);
                    bumpNested(lock, inner, nestedCells, i + seed);
                }
            }, "gcd1-mon-" + t);
        }
        for (Thread w : workers) {
            w.start();
        }
        for (Thread w : workers) {
            w.join();
        }
        long a = sum(cells);
        long b = sum(nestedCells);
        System.out.println("monitor-only-sum " + a);
        System.out.println("monitor-only-nested-sum " + b);
        boolean ok = a == (long) THREADS * ROUNDS && b == 2L * THREADS * ROUNDS;
        System.out.println(ok ? "PASS" : "FAIL");
    }
}
