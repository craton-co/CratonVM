// SPDX-License-Identifier: Apache-2.0
// JIT round 17 wave 1 (lane ffmseg): what a downcall with MemorySegment
// arguments costs per segment kind, and that the session rules stay
// HotSpot's while a call holds its segments.
//
//   java --enable-native-access=ALL-UNNAMED R17FfmsegCost [reps]
//
// stdout, one timing token per line ("<row> <n> ns/op", the median of reps x
// N calls through a compiled loop; labels carry no digits), then facts that
// match HotSpot 25 byte for byte. Rows:
//   llabs            a plain downcall (no segment): the floor
//   strlen-confined  one segment of a confined arena (its session acquired)
//   strlen-shared    one segment of a shared arena
//   strlen-global    one segment of the global arena
//   strlen-auto      one segment of an automatic arena
//   strlen-crit      the confined segment through critical(false)
//   memcmp-three     three segments of three arenas (confined, shared,
//                    auto) plus captureCallState: three sessions per call
//   strlen-heap-crit a byte[] heap segment through critical(true)
// Facts: a checksum per row, the closed-arena IllegalStateException (confined
// and a shared arena closed by another thread), the foreign-thread
// WrongThreadException, the heap refusal without critical(true), and a
// close from an upcall (qsort's comparator) of the arena whose segment the
// running downcall holds: refused with "Session is acquired by 1 clients".
// CRATONVM_DBG_FFM=1 prints "[ffm-plain] hit ... row=" per handle (the
// segment row's engagement, CRATONVM_FFM_DOWNCALL_SEGMENT_ROW).
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.Arrays;

public class R17FfmsegCost {
    static final int N = 50_000;
    static final Linker LINKER = Linker.nativeLinker();
    static final SymbolLookup STD = LINKER.defaultLookup();
    static final FunctionDescriptor JJ = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG);
    static final FunctionDescriptor JS = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS);
    static volatile Object sink;
    static boolean bad;

    // ---- compiled loops ------------------------------------------------

    static long loopLong(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) h.invokeExact((long) -i);
            if (v != i) {
                bad = true;
            }
            sum += v;
        }
        return sum;
    }

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

    static long loopThree(MethodHandle h, MemorySegment state, MemorySegment a, MemorySegment b,
            int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            int r = (int) h.invokeExact(state, a, b, 8L);
            if (r != 0) {
                bad = true;
            }
            sum += r + 1;
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

    // ---- the upcall that tries to close a held arena ----------------------

    static Arena held;
    static String closeAnswer = "comparator never ran";

    static int closeFromComparator(MemorySegment a, MemorySegment b) {
        if (held != null) {
            Arena arena = held;
            held = null;
            try {
                arena.close();
                closeAnswer = "closed";
            } catch (Throwable t) {
                closeAnswer = t.getClass().getName() + ": " + t.getMessage();
            }
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
        return t.getClass().getName() + ": " + m;
    }

    static void fact(String label, Body body) {
        try {
            System.out.println(label + ": " + body.run(1));
        } catch (Throwable t) {
            System.out.println(label + ": threw " + describe(t));
        }
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        Arena confined = Arena.ofConfined();
        Arena shared = Arena.ofShared();
        Arena auto = Arena.ofAuto();
        MemorySegment labsAddr = STD.find("llabs").orElseThrow();
        MethodHandle labs = LINKER.downcallHandle(labsAddr, JJ);
        MemorySegment strlenAddr = STD.find("strlen").orElseThrow();
        MethodHandle strlen = LINKER.downcallHandle(strlenAddr, JS);
        MethodHandle strlenCrit = LINKER.downcallHandle(strlenAddr, JS, Linker.Option.critical(false));
        MethodHandle strlenHeap = LINKER.downcallHandle(strlenAddr, JS, Linker.Option.critical(true));
        MethodHandle memcmp = LINKER.downcallHandle(STD.find("memcmp").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS,
                        ValueLayout.JAVA_LONG),
                Linker.Option.captureCallState("errno"));

        MemorySegment conf = confined.allocateFrom("confined");
        MemorySegment shar = shared.allocateFrom("shared!");
        MemorySegment glob = Arena.global().allocateFrom("global");
        MemorySegment aut = auto.allocateFrom("auto!");
        MemorySegment state = confined.allocate(Linker.Option.captureStateLayout());
        MemorySegment left = shared.allocateFrom("abcdefgh");
        MemorySegment right = auto.allocateFrom("abcdefgh");
        MemorySegment heap = MemorySegment.ofArray("heap!\0".getBytes());

        // ---- timings ----
        time("llabs", reps, n -> loopLong(labs, n));
        time("strlen-confined", reps, n -> loopSeg(strlen, conf, 8, n));
        time("strlen-shared", reps, n -> loopSeg(strlen, shar, 7, n));
        time("strlen-global", reps, n -> loopSeg(strlen, glob, 6, n));
        time("strlen-auto", reps, n -> loopSeg(strlen, aut, 5, n));
        time("strlen-crit", reps, n -> loopSeg(strlenCrit, conf, 8, n));
        time("memcmp-three", reps, n -> loopThree(memcmp, state, left, right, n));
        time("strlen-heap-crit", reps, n -> loopSeg(strlenHeap, heap, 5, n));

        // ---- correctness, through the loops the timings compiled ----
        fact("gc-while-bound", n -> loopSegGc(strlen, conf, 5000));
        fact("heap-plain", n -> loopSeg(strlen, heap, 5, 1));
        fact("heap-critical-false", n -> loopSeg(strlenCrit, heap, 5, 1));
        fact("null-argument", n -> loopSeg(strlen, null, 0, 1));

        Object[] fromThread = new Object[1];
        Thread t = new Thread(() -> {
            try {
                fromThread[0] = loopSeg(strlen, conf, 8, 1);
            } catch (Throwable e) {
                fromThread[0] = "threw " + describe(e);
            }
        });
        t.start();
        t.join();
        System.out.println("wrong-thread: " + fromThread[0]);

        Arena closing = Arena.ofConfined();
        MemorySegment doomed = closing.allocateFrom("doomed");
        MemorySegment doomedState = closing.allocate(Linker.Option.captureStateLayout());
        fact("before-close", n -> loopSeg(strlen, doomed, 6, 1000));
        closing.close();
        fact("after-close", n -> loopSeg(strlen, doomed, 6, 1));
        fact("after-close-crit", n -> loopSeg(strlenCrit, doomed, 6, 1));
        fact("after-close-state", n -> loopThree(memcmp, doomedState, left, right, 1));
        fact("after-close-middle", n -> loopThree(memcmp, state, doomed, right, 1));

        Arena sharedClosing = Arena.ofShared();
        MemorySegment sharedDoomed = sharedClosing.allocateFrom("shared");
        fact("shared-open", n -> loopSeg(strlen, sharedDoomed, 6, 1000));
        Thread closer = new Thread(sharedClosing::close);
        closer.start();
        closer.join();
        fact("shared-after-close", n -> loopSeg(strlen, sharedDoomed, 6, 1));

        // A close from inside the call, of the arena whose segment it holds.
        MethodHandle cmp = MethodHandles.lookup().findStatic(R17FfmsegCost.class, "closeFromComparator",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        AddressLayout intPtr = ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT);
        MemorySegment cmpStub = LINKER.upcallStub(cmp,
                FunctionDescriptor.of(ValueLayout.JAVA_INT, intPtr, intPtr), Arena.global());
        MethodHandle qsort = LINKER.downcallHandle(STD.find("qsort").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        Arena victim = Arena.ofConfined();
        MemorySegment ints = victim.allocateFrom(ValueLayout.JAVA_INT, 4, 3, 2, 1);
        held = victim;
        qsort.invokeExact(ints, 4L, 4L, cmpStub);
        System.out.println("close-while-held: " + closeAnswer);
        System.out.println("sorted-after: " + Arrays.toString(ints.toArray(ValueLayout.JAVA_INT)));
        victim.close();
        System.out.println("closed-after-call: " + !victim.scope().isAlive());

        VarHandle errno = Linker.Option.captureStateLayout()
                .varHandle(MemoryLayout.PathElement.groupElement("errno"));
        state.set(ValueLayout.JAVA_INT, 0, -1);
        loopThree(memcmp, state, left, right, 1);
        System.out.println("errno-written: " + ((int) errno.get(state, 0L) != -1));

        System.out.println(bad ? "FAIL" : "PASS");
        shared.close();
        confined.close();
    }
}
