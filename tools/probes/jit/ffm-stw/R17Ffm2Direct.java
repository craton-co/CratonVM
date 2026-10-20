// SPDX-License-Identifier: Apache-2.0
// JIT round 17 wave 2 (lane ffm2): the direct segment row (FFMSEG17-1,
// CRATONVM_JIT_FFM_DOWNCALL_DIRECT_SEGMENTS), the segment row's real JDK
// segments (CRATONVM_FFM_SEGMENT_ROW_REAL_SEGMENTS), and the session rules a
// direct segment call must keep.
//
//   java --enable-native-access=ALL-UNNAMED R17Ffm2Direct [reps]
//
// stdout: one timing token per line ("<row> <n> ns/op", the median of reps x
// N calls through a compiled loop; labels carry no digits), then facts that
// match HotSpot 25 byte for byte. Rows:
//   strlen-confined   a confined native segment, critical(false) not given
//   strlen-crit-true  the same segment through critical(true)
//   llabs-errno       a plain long with captureCallState("errno")
//   strlen-mapped     a FileChannel.map segment (a real JDK segment class)
//   strlen-two-arena  strlen of a confined then a shared segment, alternating
// Facts: checksums, a collection between calls, a collection inside an
// upcall that a direct segment call (qsort) holds its session across, the
// closed and foreign-thread refusals, null, the heap refusal, the closed
// mapped arena, a capture-state segment smaller than the layout
// (IndexOutOfBoundsException; the message's segment text is cut), and the closer's Thread.getState() while another thread's
// copy is in flight (HotSpot never reports it WAITING).
// CRATONVM_DBG_FFM=1 prints "[ffm-direct] hit ... segments=true" per site.
import java.io.IOException;
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.Arrays;
import java.util.EnumSet;
import java.util.TreeSet;

public class R17Ffm2Direct {
    static final int N = 50_000;
    static final Linker LINKER = Linker.nativeLinker();
    static final SymbolLookup STD = LINKER.defaultLookup();
    static final FunctionDescriptor JS = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS);
    static volatile Object sink;
    static boolean bad;

    // ---- compiled loops ------------------------------------------------

    static long loopSeg(MethodHandle h, MemorySegment s, long expect, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long len = (long) h.invokeExact(s);
            if (len != expect) {
                bad = true;
            }
            sum += len;
        }
        return sum;
    }

    static long loopTwo(MethodHandle h, MemorySegment a, MemorySegment b, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (long) h.invokeExact((MemorySegment) ((i & 1) == 0 ? a : b));
        }
        return sum;
    }

    static long loopErrno(MethodHandle h, MemorySegment state, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) h.invokeExact(state, (long) -i);
            if (v != i) {
                bad = true;
            }
            sum += v;
        }
        return sum;
    }

    static long loopSegGc(MethodHandle h, MemorySegment s, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sink = new byte[4096];
            sum += (long) h.invokeExact(s);
        }
        return sum;
    }

    // ---- an upcall that collects while qsort holds its segment -----------

    static int gcCompares;

    static int compareAndCollect(MemorySegment a, MemorySegment b) {
        if (gcCompares++ < 2) {
            System.gc();
            sink = new byte[1 << 16];
        }
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    // ---- harness ----------------------------------------------------------

    interface Body {
        long run(int n) throws Throwable;
    }

    static void time(String label, int reps, Body body) throws Throwable {
        for (int w = 0; w < 10; w++) {
            body.run(1000);
        }
        long[] ns = new long[reps];
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            sum = body.run(N);
            ns[r] = (System.nanoTime() - t0) / N;
        }
        Arrays.sort(ns);
        System.out.println(label + " " + ns[reps / 2] + " ns/op");
        System.out.println(label + " sum: " + sum + (bad ? " WRONG" : ""));
    }

    static String describe(Throwable t) {
        String m = t.getMessage();
        if (m != null && m.startsWith("Heap segment not allowed")) {
            m = "Heap segment not allowed";
        }
        if (m != null && m.startsWith("Out of bound access") && m.contains("; new offset")) {
            m = m.substring(m.indexOf("; new offset") + 2);
        }
        return t.getClass().getName() + ": " + m;
    }

    static void fact(String label, Body body) {
        try {
            System.out.println(label + ": " + body.run(1));
        } catch (Throwable t) {
            System.out.println(label + ": threw " + describe(t));
        }
    }

    static MemorySegment mapped(Arena arena) throws IOException {
        Path file = Files.createTempFile("r17ffm", ".bin");
        file.toFile().deleteOnExit();
        Files.write(file, "mapped!\0".getBytes());
        try (FileChannel ch = FileChannel.open(file, EnumSet.of(StandardOpenOption.READ))) {
            return ch.map(FileChannel.MapMode.READ_ONLY, 0, 8, arena);
        }
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        Arena confined = Arena.ofConfined();
        Arena shared = Arena.ofShared();
        MemorySegment strlenAddr = STD.find("strlen").orElseThrow();
        MethodHandle strlen = LINKER.downcallHandle(strlenAddr, JS);
        MethodHandle strlenCrit = LINKER.downcallHandle(strlenAddr, JS, Linker.Option.critical(true));
        MethodHandle labsErrno = LINKER.downcallHandle(STD.find("llabs").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG),
                Linker.Option.captureCallState("errno"));

        MemorySegment conf = confined.allocateFrom("confined");
        MemorySegment shar = shared.allocateFrom("shared!");
        MemorySegment state = confined.allocate(Linker.Option.captureStateLayout());
        Arena mapArena = Arena.ofConfined();
        MemorySegment map = mapped(mapArena);

        // ---- timings ----
        time("strlen-confined", reps, n -> loopSeg(strlen, conf, 8, n));
        time("strlen-crit-true", reps, n -> loopSeg(strlenCrit, conf, 8, n));
        time("llabs-errno", reps, n -> loopErrno(labsErrno, state, n));
        time("strlen-mapped", reps, n -> loopSeg(strlen, map, 7, n));
        time("strlen-two-arena", reps, n -> loopTwo(strlen, conf, shar, n));

        // ---- correctness, through the loops the timings compiled ----
        fact("gc-while-bound", n -> loopSegGc(strlen, conf, 5000));
        fact("gc-while-bound-crit", n -> loopSegGc(strlenCrit, conf, 5000));
        fact("null-argument", n -> loopSeg(strlen, null, 0, 1));
        fact("heap-plain", n -> loopSeg(strlen, MemorySegment.ofArray("heap\0".getBytes()), 4, 1));
        fact("heap-crit-true", n -> loopSeg(strlenCrit, MemorySegment.ofArray("heap\0".getBytes()), 4, 1));

        Object[] fromThread = new Object[2];
        Thread t = new Thread(() -> {
            try {
                fromThread[0] = loopSeg(strlen, conf, 8, 1);
            } catch (Throwable e) {
                fromThread[0] = "threw " + describe(e);
            }
            try {
                fromThread[1] = loopSeg(strlenCrit, conf, 8, 1);
            } catch (Throwable e) {
                fromThread[1] = "threw " + describe(e);
            }
        });
        t.start();
        t.join();
        System.out.println("wrong-thread: " + fromThread[0]);
        System.out.println("wrong-thread-crit: " + fromThread[1]);

        Arena closing = Arena.ofConfined();
        MemorySegment doomed = closing.allocateFrom("doomed");
        MemorySegment doomedState = closing.allocate(Linker.Option.captureStateLayout());
        fact("before-close", n -> loopSeg(strlen, doomed, 6, 1000));
        fact("before-close-state", n -> loopErrno(labsErrno, doomedState, 1000));
        closing.close();
        fact("after-close", n -> loopSeg(strlen, doomed, 6, 1));
        fact("after-close-crit", n -> loopSeg(strlenCrit, doomed, 6, 1));
        fact("after-close-state", n -> loopErrno(labsErrno, doomedState, 1));

        MemorySegment tiny = confined.allocate(2, 8);
        fact("state-too-small", n -> loopErrno(labsErrno, tiny, 1));
        fact("state-too-small-cold", n -> {
            MethodHandle cold = LINKER.downcallHandle(STD.find("llabs").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG),
                    Linker.Option.captureCallState("errno"));
            return (long) cold.invokeExact(tiny, -3L);
        });

        mapArena.close();
        fact("mapped-after-close", n -> loopSeg(strlen, map, 7, 1));

        Arena sharedClosing = Arena.ofShared();
        MemorySegment sharedDoomed = sharedClosing.allocateFrom("shared");
        fact("shared-open", n -> loopSeg(strlen, sharedDoomed, 6, 1000));
        Thread closer = new Thread(sharedClosing::close);
        closer.start();
        closer.join();
        fact("shared-after-close", n -> loopSeg(strlen, sharedDoomed, 6, 1));

        // A collection inside an upcall while qsort (four segment-or-long
        // parameters, non-critical) holds its array's session: the session
        // must be released after the call, so the arena closes.
        MethodHandle cmp = MethodHandles.lookup().findStatic(R17Ffm2Direct.class, "compareAndCollect",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        AddressLayout intPtr = ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT);
        MemorySegment cmpStub = LINKER.upcallStub(cmp,
                FunctionDescriptor.of(ValueLayout.JAVA_INT, intPtr, intPtr), Arena.global());
        MethodHandle qsort = LINKER.downcallHandle(STD.find("qsort").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        long sortedSum = 0;
        for (int round = 0; round < 3000; round++) {
            Arena victim = Arena.ofConfined();
            MemorySegment ints = victim.allocateFrom(ValueLayout.JAVA_INT, 4, 3, 2, 1);
            if (round == 2999) {
                gcCompares = 0;
            }
            qsort.invokeExact(ints, 4L, 4L, cmpStub);
            sortedSum += ints.get(ValueLayout.JAVA_INT, 0) * 1000 + ints.get(ValueLayout.JAVA_INT, 12);
            victim.close();
            if (victim.scope().isAlive()) {
                bad = true;
            }
        }
        System.out.println("qsort-gc-upcall sum: " + sortedSum);

        // The closer of a shared arena while another thread's copy is in flight.
        Arena big = Arena.ofShared();
        MemorySegment src = big.allocate(1 << 24);
        MemorySegment dst = Arena.global().allocate(1 << 24);
        volatile_stop = false;
        Thread copier = new Thread(() -> {
            try {
                while (!volatile_stop) {
                    MemorySegment.copy(src, 0, dst, 0, 1 << 24);
                }
            } catch (IllegalStateException e) {
                // closed under it: expected
            }
        });
        copier.start();
        Thread.sleep(50);
        Thread bigCloser = new Thread(() -> {
            while (true) {
                try {
                    big.close();
                    return;
                } catch (IllegalStateException e) {
                    Thread.onSpinWait();
                }
            }
        });
        TreeSet<String> seen = new TreeSet<>();
        bigCloser.start();
        while (bigCloser.isAlive()) {
            Thread.State s = bigCloser.getState();
            if (s != Thread.State.TERMINATED) {
                seen.add(s.toString());
            }
        }
        bigCloser.join();
        volatile_stop = true;
        copier.join();
        System.out.println("closer-states-not-waiting: "
                + !(seen.contains("WAITING") || seen.contains("TIMED_WAITING")));
        System.out.println("closed-big: " + !big.scope().isAlive());

        System.out.println(bad ? "FAIL" : "PASS");
        shared.close();
        confined.close();
    }

    static volatile boolean volatile_stop;
}
