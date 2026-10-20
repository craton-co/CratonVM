// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i11/vt: virtual threads (continuations) as GC roots.
 *
 * Every virtual thread keeps object graphs in the locals of a recursion of
 * known depth, yields / parks / sleeps / parks pinned while the main thread
 * runs System.gc() and allocation pressure, and re-checksums its graphs after
 * every remount. stdout is a pure function of the arguments (no timing, no
 * ids); diagnostics go to stderr.
 *
 *   churn N R          N virtual threads, R yield rounds each, GC driver on main
 *   parked N [dump|print]
 *                      N virtual threads parked (unmounted) at a known frame
 *                      while main collects; then their stack traces (and, with
 *                      `dump`, HotSpotDiagnosticMXBean.dumpThreads = jcmd
 *                      Thread.dump_to_file) or, with `print`, the
 *                      DiagnosticCommand MBean's threadPrint (= jcmd
 *                      Thread.print) are counted; then they resume and
 *                      re-check their graphs
 *   unreachable        a parked virtual thread nobody references: is it (and
 *                      an object only its frame holds) collected?
 */
public class GciVt11ContinuationRootsProbe {
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static final Object[] LOCKS = { new Object(), new Object(), new Object(), new Object() };
    static volatile Object sink;
    static volatile boolean release;

    static final class Node {
        final int id;
        final long[] payload;
        Node next;
        Object side;

        Node(int id, int len) {
            this.id = id;
            this.payload = new long[len];
            for (int i = 0; i < len; i++) {
                payload[i] = (long) id * 31L + i;
            }
        }
    }

    static Node build(int seed, int n) {
        Node head = null;
        for (int i = n - 1; i >= 0; i--) {
            Node x = new Node(seed * 1000 + i, 4 + (i % 13));
            x.next = head;
            if (i % 3 == 0) {
                x.side = "s" + seed + "_" + i;
            } else {
                x.side = new int[] { seed, i };
            }
            head = x;
        }
        return head;
    }

    static long sum(Node h) {
        long s = 0;
        for (Node x = h; x != null; x = x.next) {
            s = s * 1_000_003L + x.id;
            for (long v : x.payload) {
                s += v;
            }
            Object side = x.side;
            if (side instanceof String) {
                s += ((String) side).hashCode();
            } else if (side instanceof int[]) {
                int[] a = (int[]) side;
                s += a[0] * 7L + a[1];
            } else {
                s += 999_999_937L;
            }
        }
        return s;
    }

    static void garbage(int k) {
        for (int i = 0; i < k; i++) {
            sink = new byte[256 + (i * 37 % 1024)];
        }
    }

    // ------------------------------------------------------------------ churn

    static long work(int idx, int rounds, int depth) {
        Node mine = build(idx, 8 + depth);
        long expect = sum(mine);
        long acc;
        if (depth > 0) {
            acc = work(idx, rounds, depth - 1);
        } else {
            acc = leaf(idx, rounds);
        }
        long got = sum(mine);
        if (got != expect) {
            MISMATCH.incrementAndGet();
            System.err.println("[vt11] churn mismatch idx=" + idx + " depth=" + depth);
        }
        return acc * 31L + got;
    }

    static long leaf(int idx, int rounds) {
        long acc = 0;
        Node a = build(idx + 7, 20);
        long ea = sum(a);
        for (int r = 0; r < rounds; r++) {
            garbage(64);
            switch ((idx + r) % 4) {
                case 0:
                    Thread.yield();
                    break;
                case 1:
                    LockSupport.parkNanos(20_000L);
                    break;
                case 2:
                    try {
                        Thread.sleep(1);
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                    break;
                default:
                    synchronized (LOCKS[idx % LOCKS.length]) {
                        LockSupport.parkNanos(10_000L);
                    }
                    break;
            }
            long ga = sum(a);
            if (ga != ea) {
                MISMATCH.incrementAndGet();
                System.err.println("[vt11] leaf mismatch idx=" + idx + " round=" + r);
            }
            acc = acc * 17L + ga + r;
            if (r % 5 == 4) {
                a = build(idx + r, 20);
                ea = sum(a);
            }
        }
        return acc;
    }

    static boolean anyAlive(List<Thread> ts) {
        for (Thread t : ts) {
            if (t.isAlive()) {
                return true;
            }
        }
        return false;
    }

    static void churn(int n, int rounds) throws Exception {
        long[] results = new long[n];
        List<Thread> ts = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            final int idx = i;
            ts.add(Thread.ofVirtual().name("vt11-" + i).start(() -> {
                results[idx] = work(idx, rounds, idx % 6);
            }));
        }
        int gcs = 0;
        while (anyAlive(ts)) {
            System.gc();
            gcs++;
            for (int k = 0; k < 2000; k++) {
                sink = new Object[16];
            }
            Thread.sleep(2);
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        for (long r : results) {
            total = total * 1_000_003L + r;
        }
        System.out.println("churn threads=" + n + " rounds=" + rounds
                + " mismatches=" + MISMATCH.get() + " checksum=" + total);
        System.err.println("[vt11] churn gcs=" + gcs);
        // gci i47w lane oz47 (vg13_d_vt_churn_g1 printed a wrong checksum with
        // mismatches=0 and rc 0): with VT11_VERIFY_RESULTS set, recompute every
        // thread's result on main after the stdout line (work() is a pure
        // function of its arguments) and name each one that differs. stderr
        // only; unset (every list row), nothing runs.
        if (System.getenv("VT11_VERIFY_RESULTS") != null) {
            int bad = 0;
            for (int i = 0; i < n; i++) {
                long want = work(i, rounds, i % 6);
                if (results[i] != want) {
                    bad++;
                    System.err.println("[vt11] churn result-differs idx=" + i + " depth=" + (i % 6)
                            + " got=" + results[i] + " want=" + want + " alive=" + ts.get(i).isAlive());
                }
            }
            System.err.println("[vt11] churn results-verified differing=" + bad);
        }
    }

    // ----------------------------------------------------------------- parked

    static long parkedBody(int idx, int depth, CountDownLatch latch) {
        Node mine = build(idx, 6);
        long e = sum(mine);
        long acc;
        if (depth > 0) {
            acc = parkedBody(idx, depth - 1, latch);
        } else {
            acc = parkPoint(idx, latch);
        }
        if (sum(mine) != e) {
            MISMATCH.incrementAndGet();
            System.err.println("[vt11] parked frame mismatch idx=" + idx + " depth=" + depth);
        }
        return acc * 13L + e;
    }

    static long parkPoint(int idx, CountDownLatch latch) {
        Node a = build(idx + 100, 30);
        long ea = sum(a);
        latch.countDown();
        while (!release) {
            LockSupport.park();
        }
        if (sum(a) != ea) {
            MISMATCH.incrementAndGet();
            System.err.println("[vt11] parkPoint mismatch idx=" + idx);
        }
        return ea;
    }

    static void parked(int n, boolean dump, boolean print) throws Exception {
        CountDownLatch latch = new CountDownLatch(n);
        long[] results = new long[n];
        List<Thread> ts = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            final int idx = i;
            ts.add(Thread.ofVirtual().name("vt11p-" + i).start(() -> {
                results[idx] = parkedBody(idx, idx % 5, latch);
            }));
        }
        latch.await();
        int waiting = 0;
        for (int spin = 0; spin < 20_000; spin++) {
            waiting = 0;
            for (Thread t : ts) {
                if (t.getState() == Thread.State.WAITING) {
                    waiting++;
                }
            }
            if (waiting == n) {
                break;
            }
            Thread.sleep(1);
        }
        System.err.println("[vt11] parked waiting=" + waiting + "/" + n);
        // Every continuation is unmounted now: move what only their frames hold.
        for (int g = 0; g < 6; g++) {
            garbage(4000);
            System.gc();
        }
        int parkPoints = 0;
        int bodies = 0;
        for (Thread t : ts) {
            for (StackTraceElement e : t.getStackTrace()) {
                if (!"GciVt11ContinuationRootsProbe".equals(e.getClassName())) {
                    continue;
                }
                if ("parkPoint".equals(e.getMethodName())) {
                    parkPoints++;
                } else if ("parkedBody".equals(e.getMethodName())) {
                    bodies++;
                }
            }
        }
        int expectBodies = 0;
        for (int i = 0; i < n; i++) {
            expectBodies += (i % 5) + 1;
        }
        System.out.println("parked stack parkPoint=" + parkPoints + "/" + n
                + " parkedBody=" + bodies + "/" + expectBodies);
        long virtualListed = 0;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t.isVirtual()) {
                virtualListed++;
            }
        }
        System.out.println("getAllStackTraces virtual=" + virtualListed);
        if (dump) {
            com.sun.management.HotSpotDiagnosticMXBean bean = ManagementFactory
                    .getPlatformMXBean(com.sun.management.HotSpotDiagnosticMXBean.class);
            Path dir = Files.createTempDirectory("vt11dump").toAbsolutePath();
            Path file = dir.resolve("threads.txt");
            int dumped = -1;
            try {
                bean.dumpThreads(file.toString(),
                        com.sun.management.HotSpotDiagnosticMXBean.ThreadDumpFormat.TEXT_PLAIN);
                dumped = 0;
                for (String line : Files.readAllLines(file)) {
                    if (line.contains("GciVt11ContinuationRootsProbe.parkPoint(")) {
                        dumped++;
                    }
                }
            } catch (Exception e) {
                System.err.println("[vt11] dumpThreads failed: " + e);
            } finally {
                Files.deleteIfExists(file);
                Files.deleteIfExists(dir);
            }
            System.out.println("dump parkPoint=" + dumped + "/" + n);
        }
        if (print) {
            // jcmd Thread.print, through the DiagnosticCommand MBean: HotSpot
            // lists platform threads only, so no thread header names one of
            // these virtual threads.
            int listed = -1;
            try {
                javax.management.MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
                javax.management.ObjectName on =
                        new javax.management.ObjectName("com.sun.management:type=DiagnosticCommand");
                Object out = mbs.invoke(on, "threadPrint", new Object[] { new String[0] },
                        new String[] { String[].class.getName() });
                listed = 0;
                for (String line : String.valueOf(out).split("\n")) {
                    if (line.startsWith("\"vt11p-")) {
                        listed++;
                    }
                }
            } catch (Exception e) {
                System.err.println("[vt11] threadPrint failed: " + e);
            }
            System.out.println("thread.print virtual listed=" + listed);
        }
        // More collections between the stack walks and the remount.
        for (int g = 0; g < 3; g++) {
            garbage(4000);
            System.gc();
        }
        release = true;
        for (Thread t : ts) {
            LockSupport.unpark(t);
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        for (long r : results) {
            total = total * 1_000_003L + r;
        }
        System.out.println("parked threads=" + n + " mismatches=" + MISMATCH.get()
                + " checksum=" + total);
    }

    // ------------------------------------------------------------ unreachable

    static WeakReference<Object> heldOnlyByFrame;

    static WeakReference<Thread> startForgotten() throws Exception {
        CountDownLatch started = new CountDownLatch(1);
        Thread t = Thread.ofVirtual().name("vt11-forgotten").unstarted(() -> {
            Object mine = new Object();
            heldOnlyByFrame = new WeakReference<>(mine);
            started.countDown();
            while (true) {
                LockSupport.park(mine);
            }
        });
        t.start();
        started.await();
        for (int i = 0; i < 20_000 && t.getState() != Thread.State.WAITING; i++) {
            Thread.sleep(1);
        }
        System.err.println("[vt11] forgotten state=" + t.getState());
        return new WeakReference<>(t);
    }

    static void unreachable() throws Exception {
        WeakReference<Thread> thread = startForgotten();
        for (int g = 0; g < 3; g++) {
            garbage(2000);
            System.gc();
            Thread.sleep(10);
        }
        System.out.println("unreachable parked thread collected=" + (thread.get() == null)
                + " frame-only object collected=" + (heldOnlyByFrame.get() == null));
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "churn";
        switch (mode) {
            case "churn":
                churn(args.length > 1 ? Integer.parseInt(args[1]) : 200,
                        args.length > 2 ? Integer.parseInt(args[2]) : 30);
                break;
            case "parked":
                parked(args.length > 1 ? Integer.parseInt(args[1]) : 100,
                        args.length > 2 && "dump".equals(args[2]),
                        args.length > 2 && "print".equals(args[2]));
                break;
            case "unreachable":
                unreachable();
                break;
            default:
                throw new IllegalArgumentException("mode: churn | parked | unreachable");
        }
    }
}
