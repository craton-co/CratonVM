// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.StructLayout;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.Arrays;
import java.util.Optional;

/**
 * gcd d5/f (2026-09-28): a thread blocked inside an FFM downcall must not hold
 * up a collection on another thread
 * ({@code docs/known-issues/jit/r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world-20260928.md};
 * adapted from JIT round 13's {@code R13Ffm3DowncallBlocksGc}).
 *
 * <p>HotSpot runs a (non-critical) downcall in {@code _thread_in_native}: a
 * safepoint does not wait for it. CratonVM's downcall stays a counted mutator
 * for the whole C call unless {@code CRATONVM_FFM_DOWNCALL_GC_SAFE=1}, so a
 * pause waits until the C function returns.
 *
 * <p>Four parts, each printing a deterministic line HotSpot prints too:
 * <ol>
 *   <li>{@code warm sum 199990000}: 20 000 {@code llabs} downcalls (the calling
 *       method gets compiled);</li>
 *   <li>{@code critical sum 199990000}: the same through a
 *       {@code Linker.Option.critical(false)} handle, which stays a counted
 *       mutator under the flag, as on HotSpot;</li>
 *   <li>{@code qsort with allocating upcalls: sorted} and
 *       {@code upcalls survived a collection: true}: {@code qsort} with a Java
 *       comparator that allocates and runs one {@code System.gc()} from inside
 *       the upcall -- under the flag the upcall leaves the downcall's GC-safe
 *       region for its Java and goes back afterwards;</li>
 *   <li>{@code captured call state: ok}: {@code Linker.Option.captureCallState}
 *       returns the C call's own state -- Linux {@code errno} after
 *       {@code strtol} overflows ({@code ERANGE}), Windows
 *       {@code GetLastError} after {@code SetLastError(1234)} (CratonVM
 *       wrote nothing on Windows and read Linux's {@code errno} late before
 *       gcd d5/f);</li>
 *   <li>{@code gc finished while the worker was in C: true} and
 *       {@code worker returned 0}: a worker sleeps in C ({@code sleep(2)} /
 *       {@code Sleep(1500)}); main collects while it is there.</li>
 * </ol>
 * The last line is {@code PASS} (or {@code FAIL <what>}); the run ends on its
 * own (the longest wait is the 2 s C sleep). stderr carries VM-independent
 * cost lines, {@code [gcd5-ffm-cost] <case> median=<ns>ns/call}, for the
 * flip gate's A/B (flag off vs on, interleaved runs).
 *
 * <p>Commands:
 * <pre>
 *   javac -d /tmp/gcd5ffm tools/bench/Gcd5FfmDowncallBlocksGcProbe.java
 *   java -XX:+UseSerialGC --enable-native-access=ALL-UNNAMED -cp /tmp/gcd5ffm Gcd5FfmDowncallBlocksGcProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC --enable-native-access=ALL-UNNAMED -cp /tmp/gcd5ffm Gcd5FfmDowncallBlocksGcProbe
 *   CRATONVM_FFM_DOWNCALL_GC_SAFE=1 cratonvm ... (same)
 * </pre>
 * Expected: HotSpot and the {@code CRATONVM_FFM_DOWNCALL_GC_SAFE=1} arm print
 * the eight lines ending {@code PASS} (3/3, each collector); the default arm
 * prints {@code gc finished while the worker was in C: false} and
 * {@code FAIL stall} (the defect, as evidence).
 */
public class Gcd5FfmDowncallBlocksGcProbe {
    static volatile boolean inCall;
    static volatile boolean returned;
    static volatile int workerResult = -1;
    static volatile Object sink;
    static int upcalls;
    static boolean gcInUpcall;
    static boolean gcInUpcallDone;

    static final int WARM = 20_000;
    static final int REPS = 5;

    static long labsLoop(MethodHandle labs, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) labs.invokeExact((long) -i);
            if (v != i) {
                System.out.println("MISMATCH labs i=" + i + " got=" + v);
            }
            sum += v;
        }
        return sum;
    }

    static void cost(String name, MethodHandle labs) throws Throwable {
        long[] ns = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            labsLoop(labs, WARM);
            ns[r] = (System.nanoTime() - t0) / WARM;
        }
        long[] sorted = ns.clone();
        Arrays.sort(sorted);
        System.err.println("[gcd5-ffm-cost] " + name + " median=" + sorted[REPS / 2]
                + "ns/call reps=" + Arrays.toString(ns));
    }

    /** qsort comparator: allocates, and collects once from inside an upcall. */
    static int compare(MemorySegment a, MemorySegment b) {
        upcalls++;
        sink = new byte[4096];
        if (gcInUpcall && !gcInUpcallDone && upcalls >= 32) {
            gcInUpcallDone = true;
            System.gc();
        }
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    public static void main(String[] args) throws Throwable {
        String failure = null;
        Linker linker = Linker.nativeLinker();
        SymbolLookup std = linker.defaultLookup();
        FunctionDescriptor labsDesc =
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG);
        MemorySegment labsAddr = std.find("llabs").orElseThrow();
        MethodHandle labs = linker.downcallHandle(labsAddr, labsDesc);
        MethodHandle labsCritical =
                linker.downcallHandle(labsAddr, labsDesc, Linker.Option.critical(false));
        System.out.println("warm sum " + labsLoop(labs, WARM));
        System.out.println("critical sum " + labsLoop(labsCritical, WARM));
        cost("llabs", labs);
        cost("llabs-critical", labsCritical);

        // qsort with an allocating comparator that collects once.
        MethodHandle qsort = linker.downcallHandle(std.find("qsort").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        MethodHandle cmp = MethodHandles.lookup().findStatic(Gcd5FfmDowncallBlocksGcProbe.class,
                "compare", MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        FunctionDescriptor cmpDesc = FunctionDescriptor.of(ValueLayout.JAVA_INT,
                ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT),
                ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT));
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment stub = linker.upcallStub(cmp, cmpDesc, arena);
            int n = 256;
            MemorySegment array = arena.allocate(ValueLayout.JAVA_INT, n);
            for (int i = 0; i < n; i++) {
                array.setAtIndex(ValueLayout.JAVA_INT, i, (i * 7919) % n);
            }
            gcInUpcall = true;
            qsort.invokeExact(array, (long) n, 4L, stub);
            gcInUpcall = false;
            boolean sorted = true;
            for (int i = 0; i < n; i++) {
                if (array.getAtIndex(ValueLayout.JAVA_INT, i) != i) {
                    sorted = false;
                }
            }
            System.out.println("qsort with allocating upcalls: " + (sorted ? "sorted" : "NOT sorted"));
            System.out.println("upcalls survived a collection: " + gcInUpcallDone);
            if (!sorted || !gcInUpcallDone) {
                failure = "qsort";
            }
        }

        // The captured call state is the C call's own.
        boolean windows = System.getProperty("os.name").toLowerCase().contains("win");
        StructLayout capLayout = Linker.Option.captureStateLayout();
        String stateName = windows ? "GetLastError" : "errno";
        VarHandle stateHandle =
                capLayout.varHandle(MemoryLayout.PathElement.groupElement(stateName));
        int expectedState;
        int capturedState;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment capture = arena.allocate(capLayout);
            if (windows) {
                MethodHandle setLastError = linker.downcallHandle(
                        SymbolLookup.libraryLookup("kernel32", arena).find("SetLastError").orElseThrow(),
                        FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT),
                        Linker.Option.captureCallState(stateName));
                setLastError.invokeExact(capture, 1234);
                expectedState = 1234;
            } else {
                MethodHandle strtol = linker.downcallHandle(std.find("strtol").orElseThrow(),
                        FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS,
                                ValueLayout.ADDRESS, ValueLayout.JAVA_INT),
                        Linker.Option.captureCallState(stateName));
                MemorySegment digits = arena.allocateFrom("999999999999999999999999");
                long ignored = (long) strtol.invokeExact(capture, digits, MemorySegment.NULL, 10);
                expectedState = 34; // ERANGE
            }
            capturedState = (int) stateHandle.get(capture, 0L);
        }
        System.out.println("captured call state: "
                + (capturedState == expectedState ? "ok" : "WRONG " + capturedState));
        if (capturedState != expectedState && failure == null) {
            failure = "capture";
        }

        // The stall: a worker sleeps in C while main collects.
        MethodHandle sleeper;
        if (windows) {
            Optional<MemorySegment> sleep =
                    SymbolLookup.libraryLookup("kernel32", Arena.global()).find("Sleep");
            sleeper = linker.downcallHandle(sleep.orElseThrow(),
                    FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        } else {
            sleeper = linker.downcallHandle(std.find("sleep").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        }
        final MethodHandle call = sleeper;
        Thread worker = new Thread(() -> {
            try {
                inCall = true;
                if (windows) {
                    call.invokeExact(1500);
                    workerResult = 0;
                } else {
                    workerResult = (int) call.invokeExact(2);
                }
            } catch (Throwable t) {
                workerResult = -2;
            }
            returned = true;
        });
        worker.start();
        while (!inCall) {
            Thread.onSpinWait();
        }
        Thread.sleep(200);
        for (int i = 0; i < 1000; i++) {
            sink = new byte[1024];
        }
        System.gc();
        boolean stillInC = !returned;
        System.out.println("gc finished while the worker was in C: " + stillInC);
        worker.join();
        System.out.println("worker returned " + workerResult);
        if (!stillInC && failure == null) {
            failure = "stall";
        }
        if (workerResult != 0 && failure == null) {
            failure = "worker";
        }
        System.out.println(failure == null ? "PASS" : "FAIL " + failure);
    }
}
