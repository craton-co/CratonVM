// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 13 (lane ffmseg): cost and correctness of the FFM memo
// paths and the JIT's bound downcall row for MemorySegment arguments,
// captureCallState and Linker.Option.critical(true) heap access.
//
//   java --enable-native-access=ALL-UNNAMED R16FfmsegBind [reps]
//
// stderr: one timing token per line, "[ffmseg-time] <label> <ns/call>"
//   (median of reps x 20 000 calls, each row a compiled loop; labels have no
//   digits). Rows: llabs getpid llabs-crit llabs-errno strlen strlen-crit
//   strlen-global strlen-auto strlen-heap-crit.
// stdout: facts only, identical to HotSpot's (write the ref there):
//   loop results, the closed-arena IllegalStateException, the foreign-thread
//   WrongThreadException, the heap-segment refusals (plain and critical(false))
//   and critical(true) acceptance with write-back, a null argument, a
//   zero-length ofAddress segment, captureCallState errno (Linux: close(-1)),
//   and collections while bound.
// CRATONVM_DBG_FFM=1 prints "[ffm-bind] site ... bound=" per fill and
// "[ffm-plain] memo/hit" lines; CRATONVM_DBG_JITC=1 shows the loops compiled.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.VarHandle;
import java.util.Arrays;

public class R16FfmsegBind {
    static final int N = 20_000;
    static final Linker LINKER = Linker.nativeLinker();
    static final SymbolLookup STD = LINKER.defaultLookup();
    static final boolean WINDOWS = System.getProperty("os.name").toLowerCase().contains("win");
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

    static long loopPid(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) h.invokeExact() == 0 ? 0 : 1;
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

    static long loopSegGc(MethodHandle h, MemorySegment s, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sink = new byte[4096];
            sum += (long) h.invokeExact(s);
        }
        return sum;
    }

    static int loopCloseErrno(MethodHandle close, MemorySegment state, VarHandle errno, int n)
            throws Throwable {
        int hits = 0;
        for (int i = 0; i < n; i++) {
            int rc = (int) close.invokeExact(state, -1);
            if (rc == -1 && (int) errno.get(state, 0L) == 9) {
                hits++;
            }
        }
        return hits;
    }

    static void loopFree(MethodHandle free, MemorySegment p, int n) throws Throwable {
        for (int i = 0; i < n; i++) {
            free.invokeExact(p);
        }
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
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            body.run(N);
            ns[r] = (System.nanoTime() - t0) / N;
        }
        Arrays.sort(ns);
        System.err.println("[ffmseg-time] " + label + " " + ns[reps / 2]);
        System.out.println(label + ": " + (bad ? "WRONG" : "ok"));
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

    static boolean closedFact(String label, Body body) {
        try {
            System.out.println(label + ": " + body.run(1));
            return false;
        } catch (Throwable t) {
            System.out.println(label + ": threw " + describe(t));
            return t instanceof IllegalStateException
                    && "Already closed".equals(t.getMessage());
        }
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        Arena arena = Arena.ofConfined();
        MemorySegment labsAddr = STD.find("llabs").orElseThrow();
        MethodHandle labs = LINKER.downcallHandle(labsAddr, JJ);
        MethodHandle labsCrit = LINKER.downcallHandle(labsAddr, JJ, Linker.Option.critical(false));
        MethodHandle labsErrno = LINKER.downcallHandle(labsAddr, JJ,
                Linker.Option.captureCallState(WINDOWS ? "GetLastError" : "errno"));
        MethodHandle pid = WINDOWS
                ? LINKER.downcallHandle(SymbolLookup.libraryLookup("kernel32", arena)
                        .find("GetCurrentProcessId").orElseThrow(), FunctionDescriptor.of(ValueLayout.JAVA_INT))
                : LINKER.downcallHandle(STD.find("getpid").orElseThrow(), FunctionDescriptor.of(ValueLayout.JAVA_INT));
        MemorySegment strlenAddr = STD.find("strlen").orElseThrow();
        MethodHandle strlen = LINKER.downcallHandle(strlenAddr, JS);
        MethodHandle strlenCrit = LINKER.downcallHandle(strlenAddr, JS, Linker.Option.critical(false));
        MethodHandle strlenHeap = LINKER.downcallHandle(strlenAddr, JS, Linker.Option.critical(true));
        MethodHandle memset = LINKER.downcallHandle(STD.find("memset").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.JAVA_INT,
                        ValueLayout.JAVA_LONG),
                Linker.Option.critical(true));
        MethodHandle free = LINKER.downcallHandle(STD.find("free").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));

        MemorySegment state = arena.allocate(Linker.Option.captureStateLayout());
        MemorySegment hello = arena.allocateFrom("hello");
        MemorySegment global = Arena.global().allocateFrom("global!");
        MemorySegment auto = Arena.ofAuto().allocateFrom("auto");
        MemorySegment heapHi = MemorySegment.ofArray("hi\0".getBytes());

        // ---- timings (stderr) and their loop facts (stdout) ----
        time("llabs", reps, n -> loopLong(labs, n));
        time("getpid", reps, n -> loopPid(pid, n));
        time("llabs-crit", reps, n -> loopLong(labsCrit, n));
        time("llabs-errno", reps, n -> loopErrno(labsErrno, state, n));
        time("strlen", reps, n -> loopSeg(strlen, hello, 5, n));
        time("strlen-crit", reps, n -> loopSeg(strlenCrit, hello, 5, n));
        time("strlen-global", reps, n -> loopSeg(strlen, global, 7, n));
        time("strlen-auto", reps, n -> loopSeg(strlen, auto, 4, n));
        time("strlen-heap-crit", reps, n -> loopSeg(strlenHeap, heapHi, 2, n));

        // ---- correctness (stdout; every call below goes through a loop
        // that the timing rows compiled and bound) ----
        fact("gc-while-bound", n -> loopSegGc(strlen, hello, 5000));
        fact("zero-length-of-address",
                n -> loopSeg(strlen, MemorySegment.ofAddress(hello.address()), 5, 100));
        fact("null-argument", n -> loopSeg(strlen, null, 0, 1));
        fact("heap-plain", n -> loopSeg(strlen, heapHi, 2, 1));
        fact("heap-critical-false", n -> loopSeg(strlenCrit, heapHi, 2, 1));
        fact("heap-critical-true", n -> loopSeg(strlenHeap, heapHi, 2, 1));
        byte[] buf = new byte[6];
        fact("heap-critical-true-write", n -> {
            MemorySegment ignored = (MemorySegment) memset.invokeExact(MemorySegment.ofArray(buf), 0x41, 4L);
            return buf[0] == 'A' && buf[3] == 'A' && buf[4] == 0 ? 1 : 0;
        });
        System.out.println("heap-critical-true-bytes: " + Arrays.toString(buf));
        fact("free-null", n -> {
            loopFree(free, MemorySegment.NULL, 100);
            return 1;
        });

        // A confined arena's segment from another thread.
        Object[] fromThread = new Object[1];
        Thread t = new Thread(() -> {
            try {
                fromThread[0] = loopSeg(strlen, hello, 5, 1);
            } catch (Throwable e) {
                fromThread[0] = "threw " + describe(e);
            }
        });
        t.start();
        t.join();
        System.out.println("wrong-thread: " + fromThread[0]);

        // A closed arena's segment, through the bound loop.
        Arena closing = Arena.ofConfined();
        MemorySegment doomed = closing.allocateFrom("doomed");
        fact("before-close", n -> loopSeg(strlen, doomed, 6, 1000));
        closing.close();
        boolean closedConfined = closedFact("after-close", n -> loopSeg(strlen, doomed, 6, 1));
        boolean closedConfinedCritical = closedFact("after-close-crit", n -> loopSeg(strlenCrit, doomed, 6, 1));
        // A shared arena closed by another thread.
        Arena shared = Arena.ofShared();
        MemorySegment sharedSeg = shared.allocateFrom("shared");
        fact("shared-open", n -> loopSeg(strlen, sharedSeg, 6, 1000));
        Thread closer = new Thread(shared::close);
        closer.start();
        closer.join();
        boolean closedShared = closedFact("shared-after-close", n -> loopSeg(strlen, sharedSeg, 6, 1));

        if (!WINDOWS) {
            MethodHandle close = LINKER.downcallHandle(STD.find("close").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT),
                    Linker.Option.captureCallState("errno"));
            VarHandle errno = Linker.Option.captureStateLayout()
                    .varHandle(MemoryLayout.PathElement.groupElement("errno"));
            fact("errno-close", n -> {
                int total = 0;
                for (int r = 0; r < 20; r++) {
                    total += loopCloseErrno(close, state, errno, 1000);
                }
                return total;
            });
        }
        System.out.println(bad || !closedConfined || !closedConfinedCritical || !closedShared
                ? "FAIL" : "PASS");
        arena.close();
    }
}
