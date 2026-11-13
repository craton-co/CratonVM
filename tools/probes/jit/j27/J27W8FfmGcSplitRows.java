// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 8 (lane W8FFM): splits R22FfmCompiledGc's wall time into the FFM work and the
// main thread's share of the churn thread's forced collections.
// Page: docs/feature-designs/native-dispatch-architecture.md
//
// The shape is R22FfmCompiledGc's (hot llabs downcalls, plain and critical, a qsort whose
// comparator is an allocating up-call, a churn thread allocating 4 KiB arrays), with two
// switches so one binary measures both halves:
//   -Dj27.churnGc=false   the churn thread never calls System.gc() (it still allocates);
//   -Dj27.upcallGc=false  the comparator never calls System.gc().
//   -Dj27.churn=false     no churn thread at all (the main thread alone: the FFM work itself).
// The facts do not depend on either switch.
//
//   java --enable-native-access=ALL-UNNAMED -Xmx256m -cp <dir> J27W8FfmGcSplitRows [rounds]
//
// Expected HotSpot 25 (any rounds >= 1, any switch):
//   fact llabs-held 19999900000
//   fact llabs-critical-held 19999900000
//   fact qsort-unsorted 0
//   fact held-boxes-ok true
//   fact upcalls-positive true
//
// -Dj27.time=true adds, per run:
//   time main-ms <wall ms of the main thread's rounds>
//   time gc-count <collections the GarbageCollectorMXBeans counted over the rounds>
//   time forced-gc <System.gc() calls the churn thread and the comparator made over the rounds>
//   time ms-per-forced-gc <main-ms / forced-gc, 0 when none>
//   time plain-ms, critical-ms, qsort-ms <wall ms of each phase over the rounds>
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.concurrent.atomic.AtomicLong;
import static java.lang.foreign.ValueLayout.*;

public class J27W8FfmGcSplitRows {
    static final Linker L = Linker.nativeLinker();
    static final SymbolLookup STD = L.defaultLookup();
    static final MethodHandle LLABS = L.downcallHandle(STD.find("llabs").orElseThrow(),
        FunctionDescriptor.of(JAVA_LONG, JAVA_LONG));
    static final MethodHandle LLABS_C = L.downcallHandle(STD.find("llabs").orElseThrow(),
        FunctionDescriptor.of(JAVA_LONG, JAVA_LONG), Linker.Option.critical(false));
    static final MethodHandle QSORT = L.downcallHandle(STD.find("qsort").orElseThrow(),
        FunctionDescriptor.ofVoid(JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG));
    static final MethodHandle MALLOC = L.downcallHandle(STD.find("malloc").orElseThrow(),
        FunctionDescriptor.of(JAVA_LONG, JAVA_LONG));
    static final MethodHandle FREE = L.downcallHandle(STD.find("free").orElseThrow(),
        FunctionDescriptor.ofVoid(JAVA_LONG));

    static final boolean CHURN_GC = Boolean.parseBoolean(System.getProperty("j27.churnGc", "true"));
    static final boolean UPCALL_GC = Boolean.parseBoolean(System.getProperty("j27.upcallGc", "true"));
    static final boolean CHURN = Boolean.parseBoolean(System.getProperty("j27.churn", "true"));
    static final AtomicLong FORCED = new AtomicLong();

    static volatile Object sink;
    static volatile boolean stop;
    static int upcalls;
    static long badBoxes;

    static int cmp(MemorySegment a, MemorySegment b) {
        int x = a.reinterpret(4).get(JAVA_INT, 0), y = b.reinterpret(4).get(JAVA_INT, 0);
        sink = new int[16 + (x & 15)];
        if (++upcalls % 20_000 == 0 && UPCALL_GC) {
            FORCED.incrementAndGet();
            System.gc();
        }
        return Integer.compare(x, y);
    }

    static final class Box {
        final long a, b;
        final String s;
        Box(long a) { this.a = a; this.b = a * 3 + 1; this.s = "v" + (a % 97); }
        boolean ok(long a) { return this.a == a && b == a * 3 + 1 && s.equals("v" + (a % 97)); }
    }

    static long llabsHeld(int n, boolean critical) throws Throwable {
        long acc = 0;
        Box held = new Box(0);
        for (int i = 0; i < n; i++) {
            if ((i & 1023) == 0) held = new Box(i);
            long r = critical ? (long) LLABS_C.invokeExact((long) -i) : (long) LLABS.invokeExact((long) -i);
            acc += r;
            if (!held.ok(i & ~1023L)) badBoxes++;
        }
        return acc;
    }

    /** The count of adjacent pairs qsort left out of order (0 on a correct run). */
    static long qsortUnsorted(MemorySegment arr, long cmpAddr, int n) throws Throwable {
        long unsorted = 0;
        Box held = new Box(7);
        int seed = 12345;
        for (int i = 0; i < n; i++) {
            for (int k = 0; k < 8; k++) {
                seed = seed * 1103515245 + 12345;
                arr.setAtIndex(JAVA_INT, k, (seed >>> 8) % 1000);
            }
            QSORT.invokeExact(arr.address(), 8L, 4L, cmpAddr);
            for (int k = 1; k < 8; k++) if (arr.getAtIndex(JAVA_INT, k - 1) > arr.getAtIndex(JAVA_INT, k)) unsorted++;
            if (!held.ok(7)) badBoxes++;
            if ((i & 255) == 0) held = new Box(7);
        }
        return unsorted;
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, b.getCollectionCount());
        }
        return n;
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        boolean time = Boolean.getBoolean("j27.time");
        Thread churn = new Thread(() -> {
            long k = 0;
            while (!stop) {
                sink = new byte[1 << 12];
                if (++k % 4000 == 0 && CHURN_GC) {
                    FORCED.incrementAndGet();
                    System.gc();
                }
            }
        });
        churn.setDaemon(true);
        if (CHURN) churn.start();
        MethodHandle cmpMh = MethodHandles.lookup().findStatic(J27W8FfmGcSplitRows.class, "cmp",
            MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        long plain = 0, critical = 0, unsorted = 0;
        long gc0, forced0, t0, ms, gcs, forced, plainNs = 0, criticalNs = 0, qsortNs = 0;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment stub = L.upcallStub(cmpMh, FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS), arena);
            long buf = (long) MALLOC.invokeExact(32L);
            MemorySegment arr = MemorySegment.ofAddress(buf).reinterpret(32);
            gc0 = gcCount();
            forced0 = FORCED.get();
            t0 = System.nanoTime();
            for (int r = 0; r < rounds; r++) {
                long p0 = System.nanoTime();
                plain = llabsHeld(200_000, false);
                long p1 = System.nanoTime();
                critical = llabsHeld(200_000, true);
                long p2 = System.nanoTime();
                unsorted += qsortUnsorted(arr, stub.address(), 20_000);
                plainNs += p1 - p0;
                criticalNs += p2 - p1;
                qsortNs += System.nanoTime() - p2;
            }
            ms = (System.nanoTime() - t0) / 1_000_000;
            gcs = gcCount() - gc0;
            forced = FORCED.get() - forced0;
            FREE.invokeExact(buf);
        }
        stop = true;
        churn.join();
        System.out.println("fact llabs-held " + plain);
        System.out.println("fact llabs-critical-held " + critical);
        System.out.println("fact qsort-unsorted " + unsorted);
        System.out.println("fact held-boxes-ok " + (badBoxes == 0));
        System.out.println("fact upcalls-positive " + (upcalls > 0));
        if (time) {
            System.out.println("time main-ms " + ms);
            System.out.println("time gc-count " + gcs);
            System.out.println("time forced-gc " + forced);
            System.out.println("time ms-per-forced-gc " + (forced == 0 ? 0 : ms / forced));
            System.out.println("time plain-ms " + plainNs / 1_000_000);
            System.out.println("time critical-ms " + criticalNs / 1_000_000);
            System.out.println("time qsort-ms " + qsortNs / 1_000_000);
        }
    }
}
