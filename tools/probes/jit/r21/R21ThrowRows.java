// JIT round 21 wave 2, lane throws: what a throw costs and answers in and around compiled code.
//
// Targets (r20 handoff perf table): a throw that escapes its compiled method (R20PIRSuperCatch
// throw-error-escapes, 1812 ns vs HotSpot 227) and the caught implicit exceptions (R20PNatFastThrow
// AIOOBE / NPE / NPE over threads, 398 / 285 / 128 ns vs 17 / 65 / 1 with HotSpot's fast throw).
// CratonVM keeps stack traces by default (CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW is opt-in, the
// owner's decision), so the reference run is HotSpot with -XX:-OmitStackTraceInFastThrow: every hot
// implicit exception there carries its message and its full trace, as here.
//
// Rows (each `rowX` throws once per call and catches it; `loopX` is the timed loop around it):
//   local          throw + catch in the same method
//   caller         thrown by a callee, caught one frame up
//   up3 / up10     thrown 3 / 10 frames above the catcher
//   spliced        thrown by a tiny callee (inlined once compiled), caught in the caller
//   error-escapes  `catch (RuntimeException)` around `throw new Error()`: the throw leaves its method
//                  and is caught as Error by the caller (the R20PIRSuperCatch shape)
//   npe-local / npe-caller        implicit NPE (getfield on a null static), caught locally / in caller
//   aioobe-local / aioobe-caller  implicit AIOOBE (index 7, length 4)
//   div0-local / div0-caller      implicit ArithmeticException (/ by zero)
//   sync-instance / sync-static / sync-block  thrown out of a synchronized method / static
//                  synchronized method / synchronized block; the monitor must be free afterwards
//   nostack-up3    a writableStackTrace=false exception thrown 3 frames up: the unwind with no capture
//   prealloc-up10  one preallocated exception rethrown from 10 frames up: unwind only, no capture
//   gst-up3        up3 plus e.getStackTrace() in the handler (StackTraceElement materialisation)
//   npe-threads    4 threads, each catching implicit NPEs locally (ns/op = wall time / per-thread ops)
// Facts per row: `cold` (first call, interpreted), `hot` (the last throw of a warm loop), both as
// <class>|<message>|<frames from the throw site down to the row method, method:line>|below=<frames
// under the row method>; `badtrace` (hot throws whose top frames differ from the cold one, of 3000
// checked after the warm-up: expected 0); `result` (the loop's checksum); for the sync rows
// `monitor-held` (Thread.holdsLock after the catch: false) and `monitor-free` (another thread enters
// the monitor within 5 s: true).
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21ThrowRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx1g -cp $P R21ThrowRows                          # --jdk-only (launcher default)
//   cratonvm --compatible -Xmx1g -cp $P R21ThrowRows             # both modes: the same fact lines
//   java -XX:-OmitStackTraceInFastThrow -Xmx1g -cp $P R21ThrowRows   # HotSpot 25 reference
//   optional arguments: ops per timed rep (default 20000); `facts` prints the fact lines only.
// Arms (5 interleaved reps each, both modes):
//   def      ||
//   census   |CRATONVM_JIT_THROW_CENSUS_STATS_ONLY=1|   (THROWS21-1, this lane: no shared census
//            counter RMW per compiled catch; deciders npe-threads, npe-local, aioobe-local; facts equal)
//   edges    |CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1|   (THROW20-7: error-escapes, caller, up3,
//            up10, sync-* rows; HotSpot catches those in the compiled caller)
//   edges+i  |CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES=1|
//   fast     |CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1|  timing only, against HotSpot with the default
//            +OmitStackTraceInFastThrow (the hot implicit facts then legitimately lose message and frames)
// HotSpot expectation: every non-`time` line identical to the -XX:-OmitStackTraceInFastThrow run;
// `badtrace 0`, `monitor-held false`, `monitor-free true` on every row. HotSpot 25 time rows (order of
// magnitude, x64): local / caller / up3 ~0.5-1 us (the capture), up10 ~1-1.5 us, error-escapes ~0.3-1 us,
// the implicit rows ~0.5-1.5 us with traces, nostack-up3 / prealloc-up10 < 50 ns (no capture: C2
// unwinds in compiled code), gst-up3 ~2-4 us.

public class R21ThrowRows {
    static final String[] ROWS = {
        "local", "caller", "up3", "up10", "spliced", "error-escapes",
        "npe-local", "npe-caller", "aioobe-local", "aioobe-caller", "div0-local", "div0-caller",
        "sync-instance", "sync-static", "sync-block", "nostack-up3", "prealloc-up10", "gst-up3",
    };

    static final class Holder {
        int f;
    }

    static final class NoStack extends RuntimeException {
        NoStack() {
            super("nostack", null, false, false);
        }
    }

    static final class Inst {
        synchronized int syncThrow(int i) {
            if (i >= 0) throw new IllegalStateException("sync-instance");
            return i;
        }
    }

    static Throwable last;
    static long sink;
    static Holder nullHolder;
    static int badIndex = 7;
    static int zero = 0;
    static final int[] ARR4 = new int[4];
    static final Object LOCK = new Object();
    static final Inst INST = new Inst();
    static final RuntimeException PRE = new RuntimeException("prealloc");

    // ---------------------------------------------------------------- the rows

    static int rowLocal(int i) {
        try {
            if (i >= 0) throw new IllegalStateException("local");
            return 0;
        } catch (IllegalStateException e) {
            last = e;
            return 1;
        }
    }

    static int throwIse(int i) {
        if (i >= 0) throw new IllegalStateException("caller");
        return i;
    }

    static int rowCaller(int i) {
        try {
            return throwIse(i);
        } catch (IllegalStateException e) {
            last = e;
            return 1;
        }
    }

    static int up3a(int i) {
        return up3b(i) + 1;
    }

    static int up3b(int i) {
        return up3c(i) + 1;
    }

    static int up3c(int i) {
        if (i >= 0) throw new IllegalStateException("up3");
        return i;
    }

    static int rowUp3(int i) {
        try {
            return up3a(i);
        } catch (IllegalStateException e) {
            last = e;
            return 1;
        }
    }

    static int u1(int i) { return u2(i) + 1; }
    static int u2(int i) { return u3(i) + 1; }
    static int u3(int i) { return u4(i) + 1; }
    static int u4(int i) { return u5(i) + 1; }
    static int u5(int i) { return u6(i) + 1; }
    static int u6(int i) { return u7(i) + 1; }
    static int u7(int i) { return u8(i) + 1; }
    static int u8(int i) { return u9(i) + 1; }
    static int u9(int i) { return u10(i) + 1; }

    static int u10(int i) {
        if (i >= 0) throw new IllegalStateException("up10");
        return i;
    }

    static int rowUp10(int i) {
        try {
            return u1(i);
        } catch (IllegalStateException e) {
            last = e;
            return 1;
        }
    }

    static void tinyCheck(int i) {
        if (i != Integer.MIN_VALUE) throw new IllegalArgumentException("tiny");
    }

    static int rowSpliced(int i) {
        try {
            tinyCheck(i);
            return 0;
        } catch (IllegalArgumentException e) {
            last = e;
            return 1;
        }
    }

    static int escapeInner(int i) {
        try {
            if (i >= 0) throw new Error("escape");
            return 0;
        } catch (RuntimeException e) {
            return -1;
        }
    }

    static int rowErrorEscapes(int i) {
        try {
            return escapeInner(i);
        } catch (Error e) {
            last = e;
            return 1;
        }
    }

    static int rowNpeLocal(int i) {
        try {
            return nullHolder.f + i;
        } catch (NullPointerException e) {
            last = e;
            return 1;
        }
    }

    static int derefNull(int i) {
        return nullHolder.f + i;
    }

    static int rowNpeCaller(int i) {
        try {
            return derefNull(i);
        } catch (NullPointerException e) {
            last = e;
            return 1;
        }
    }

    static int rowAioobeLocal(int i) {
        try {
            return ARR4[badIndex] + i;
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return 1;
        }
    }

    static int loadAt(int[] a, int k) {
        return a[k];
    }

    static int rowAioobeCaller(int i) {
        try {
            return loadAt(ARR4, badIndex) + i;
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return 1;
        }
    }

    static int rowDivLocal(int i) {
        try {
            return i / zero;
        } catch (ArithmeticException e) {
            last = e;
            return 1;
        }
    }

    static int divide(int a, int b) {
        return a / b;
    }

    static int rowDivCaller(int i) {
        try {
            return divide(i, zero);
        } catch (ArithmeticException e) {
            last = e;
            return 1;
        }
    }

    static int rowSyncInstance(int i) {
        try {
            return INST.syncThrow(i);
        } catch (IllegalStateException e) {
            last = e;
            return 1;
        }
    }

    static synchronized int staticSyncThrow(int i) {
        if (i >= 0) throw new IllegalStateException("sync-static");
        return i;
    }

    static int rowSyncStatic(int i) {
        try {
            return staticSyncThrow(i);
        } catch (IllegalStateException e) {
            last = e;
            return 1;
        }
    }

    static int syncBlockThrow(int i) {
        synchronized (LOCK) {
            if (i >= 0) throw new IllegalStateException("sync-block");
            return i;
        }
    }

    static int rowSyncBlock(int i) {
        try {
            return syncBlockThrow(i);
        } catch (IllegalStateException e) {
            last = e;
            return 1;
        }
    }

    static int ns3a(int i) {
        return ns3b(i) + 1;
    }

    static int ns3b(int i) {
        return ns3c(i) + 1;
    }

    static int ns3c(int i) {
        if (i >= 0) throw new NoStack();
        return i;
    }

    static int rowNostackUp3(int i) {
        try {
            return ns3a(i);
        } catch (NoStack e) {
            last = e;
            return 1;
        }
    }

    static int p1(int i) { return p2(i) + 1; }
    static int p2(int i) { return p3(i) + 1; }
    static int p3(int i) { return p4(i) + 1; }
    static int p4(int i) { return p5(i) + 1; }
    static int p5(int i) { return p6(i) + 1; }
    static int p6(int i) { return p7(i) + 1; }
    static int p7(int i) { return p8(i) + 1; }
    static int p8(int i) { return p9(i) + 1; }
    static int p9(int i) { return p10(i) + 1; }

    static int p10(int i) {
        if (i >= 0) throw PRE;
        return i;
    }

    static int rowPreallocUp10(int i) {
        try {
            return p1(i);
        } catch (RuntimeException e) {
            last = e;
            return 1;
        }
    }

    static int rowGstUp3(int i) {
        try {
            return up3a(i);
        } catch (IllegalStateException e) {
            last = e;
            return e.getStackTrace().length;
        }
    }

    static int rowNpeThread(Throwable[] out, int i) {
        try {
            return nullHolder.f + i;
        } catch (NullPointerException e) {
            out[0] = e;
            return 1;
        }
    }

    // ---------------------------------------------------------------- loops (one per row)

    static int loopLocal(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowLocal(i); return a; }
    static int loopCaller(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowCaller(i); return a; }
    static int loopUp3(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowUp3(i); return a; }
    static int loopUp10(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowUp10(i); return a; }
    static int loopSpliced(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowSpliced(i); return a; }
    static int loopErrorEscapes(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowErrorEscapes(i); return a; }
    static int loopNpeLocal(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowNpeLocal(i); return a; }
    static int loopNpeCaller(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowNpeCaller(i); return a; }
    static int loopAioobeLocal(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowAioobeLocal(i); return a; }
    static int loopAioobeCaller(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowAioobeCaller(i); return a; }
    static int loopDivLocal(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowDivLocal(i); return a; }
    static int loopDivCaller(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowDivCaller(i); return a; }
    static int loopSyncInstance(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowSyncInstance(i); return a; }
    static int loopSyncStatic(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowSyncStatic(i); return a; }
    static int loopSyncBlock(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowSyncBlock(i); return a; }
    static int loopNostackUp3(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowNostackUp3(i); return a; }
    static int loopPreallocUp10(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowPreallocUp10(i); return a; }
    static int loopGstUp3(int n) { int a = 0; for (int i = 0; i < n; i++) a += rowGstUp3(i); return a; }

    static int loopNpeThread(Throwable[] out, int n) {
        int a = 0;
        for (int i = 0; i < n; i++) a += rowNpeThread(out, i);
        return a;
    }

    static int loop(int row, int n) {
        switch (row) {
            case 0: return loopLocal(n);
            case 1: return loopCaller(n);
            case 2: return loopUp3(n);
            case 3: return loopUp10(n);
            case 4: return loopSpliced(n);
            case 5: return loopErrorEscapes(n);
            case 6: return loopNpeLocal(n);
            case 7: return loopNpeCaller(n);
            case 8: return loopAioobeLocal(n);
            case 9: return loopAioobeCaller(n);
            case 10: return loopDivLocal(n);
            case 11: return loopDivCaller(n);
            case 12: return loopSyncInstance(n);
            case 13: return loopSyncStatic(n);
            case 14: return loopSyncBlock(n);
            case 15: return loopNostackUp3(n);
            case 16: return loopPreallocUp10(n);
            case 17: return loopGstUp3(n);
            default: throw new IllegalArgumentException("row " + row);
        }
    }

    // One call of a row, not through its loop (the cold fact and the trace check).
    static int op(int row, int i) {
        switch (row) {
            case 0: return rowLocal(i);
            case 1: return rowCaller(i);
            case 2: return rowUp3(i);
            case 3: return rowUp10(i);
            case 4: return rowSpliced(i);
            case 5: return rowErrorEscapes(i);
            case 6: return rowNpeLocal(i);
            case 7: return rowNpeCaller(i);
            case 8: return rowAioobeLocal(i);
            case 9: return rowAioobeCaller(i);
            case 10: return rowDivLocal(i);
            case 11: return rowDivCaller(i);
            case 12: return rowSyncInstance(i);
            case 13: return rowSyncStatic(i);
            case 14: return rowSyncBlock(i);
            case 15: return rowNostackUp3(i);
            case 16: return rowPreallocUp10(i);
            case 17: return rowGstUp3(i);
            default: throw new IllegalArgumentException("row " + row);
        }
    }

    static String rowMethod(int row) {
        String[] names = {
            "rowLocal", "rowCaller", "rowUp3", "rowUp10", "rowSpliced", "rowErrorEscapes",
            "rowNpeLocal", "rowNpeCaller", "rowAioobeLocal", "rowAioobeCaller", "rowDivLocal",
            "rowDivCaller", "rowSyncInstance", "rowSyncStatic", "rowSyncBlock", "rowNostackUp3",
            "rowPreallocUp10", "rowGstUp3",
        };
        return names[row];
    }

    // ---------------------------------------------------------------- trace rendering

    static String frame(StackTraceElement e, boolean withLine) {
        String cls = e.getClassName();
        String s = cls + "." + e.getMethodName();
        return withLine ? s + ":" + e.getLineNumber() : s;
    }

    // Frames from the throw site down to (and including) the frame of `rowMethod`.
    static String top(Throwable t, String rowMethod) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            if (sb.length() > 0) sb.append(',');
            sb.append(frame(e, true));
            if (e.getClassName().equals("R21ThrowRows") && e.getMethodName().equals(rowMethod)) break;
        }
        return sb.toString();
    }

    // Frames under the row method's frame: probe frames with lines, JDK frames without.
    static String below(Throwable t, String rowMethod) {
        StackTraceElement[] st = t.getStackTrace();
        int k = 0;
        while (k < st.length
                && !(st[k].getClassName().equals("R21ThrowRows") && st[k].getMethodName().equals(rowMethod))) {
            k++;
        }
        if (k >= st.length) return "none";
        StringBuilder sb = new StringBuilder();
        for (int j = k + 1; j < st.length; j++) {
            if (sb.length() > 0) sb.append(',');
            sb.append(frame(st[j], st[j].getClassName().startsWith("R21ThrowRows")));
        }
        return sb.toString();
    }

    static String describe(Throwable t, String rowMethod) {
        if (t == null) return "null";
        return t.getClass().getName() + "|" + t.getMessage() + "|" + top(t, rowMethod)
                + "|depth=" + t.getStackTrace().length + "|below=" + below(t, rowMethod);
    }

    // ---------------------------------------------------------------- monitors

    static boolean enteredByAnotherThread(Object monitor) throws InterruptedException {
        final boolean[] got = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (monitor) {
                got[0] = true;
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(5000);
        return got[0];
    }

    static Object monitorOf(int row) {
        switch (row) {
            case 12: return INST;
            case 13: return R21ThrowRows.class;
            case 14: return LOCK;
            default: return null;
        }
    }

    // ---------------------------------------------------------------- driver

    static long median(long[] v) {
        long[] c = v.clone();
        java.util.Arrays.sort(c);
        return c[c.length / 2];
    }

    static void factsAndTime(int row, int ops, boolean time) throws InterruptedException {
        String name = ROWS[row];
        String method = rowMethod(row);
        last = null;
        sink += op(row, 0);
        Throwable cold = last;
        System.out.println("fact " + name + " cold " + describe(cold, method));
        String coldTop = cold == null ? "null" : top(cold, method);
        // Warm-up through the row's own loop (compiles the row, its callees and the loop).
        int warm = 0;
        for (int r = 0; r < 4; r++) warm += loop(row, 10000);
        System.out.println("fact " + name + " result " + warm);
        Throwable hot = last;
        System.out.println("fact " + name + " hot " + describe(hot, method));
        int bad = 0;
        for (int i = 0; i < 3000; i++) {
            last = null;
            sink += op(row, i);
            String t = last == null ? "null" : top(last, method);
            if (!t.equals(coldTop)) bad++;
        }
        System.out.println("fact " + name + " badtrace " + bad);
        Object monitor = monitorOf(row);
        if (monitor != null) {
            System.out.println("fact " + name + " monitor-held " + Thread.holdsLock(monitor));
            System.out.println("fact " + name + " monitor-free " + enteredByAnotherThread(monitor));
        }
        if (!time) return;
        long[] reps = new long[9];
        for (int r = 0; r < reps.length; r++) {
            long t0 = System.nanoTime();
            sink += loop(row, ops);
            reps[r] = (System.nanoTime() - t0) / ops;
        }
        System.out.println("time " + name + " " + median(reps) + " ns/op");
    }

    static final int THREADS = 4;

    static long runThreads(int ops, int[] bad, Throwable[] lasts, String coldTop) throws InterruptedException {
        Thread[] ts = new Thread[THREADS];
        for (int k = 0; k < THREADS; k++) {
            final int id = k;
            ts[k] = new Thread(() -> {
                Throwable[] out = new Throwable[1];
                sink += loopNpeThread(out, ops);
                if (bad != null) {
                    int b = 0;
                    for (int i = 0; i < 3000; i++) {
                        out[0] = null;
                        sink += rowNpeThread(out, i);
                        String t = out[0] == null ? "null" : top(out[0], "rowNpeThread");
                        if (!t.equals(coldTop)) b++;
                    }
                    bad[id] = b;
                }
                lasts[id] = out[0];
            });
        }
        long t0 = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        return System.nanoTime() - t0;
    }

    static void threadsRow(int ops, boolean time) throws InterruptedException {
        Throwable[] out = new Throwable[1];
        sink += rowNpeThread(out, 0);
        Throwable cold = out[0];
        String coldTop = cold == null ? "null" : top(cold, "rowNpeThread");
        System.out.println("fact npe-threads cold " + cold.getClass().getName() + "|" + cold.getMessage()
                + "|" + coldTop);
        int[] bad = new int[THREADS];
        Throwable[] lasts = new Throwable[THREADS];
        runThreads(40000, bad, lasts, coldTop);
        int badSum = 0;
        for (int b : bad) badSum += b;
        System.out.println("fact npe-threads badtrace " + badSum);
        Throwable hot = lasts[0];
        System.out.println("fact npe-threads hot " + describe(hot, "rowNpeThread"));
        if (!time) return;
        long[] reps = new long[9];
        for (int r = 0; r < reps.length; r++) {
            reps[r] = runThreads(ops, null, lasts, coldTop) / ops;
        }
        System.out.println("time npe-threads " + median(reps) + " ns/op");
    }

    public static void main(String[] args) throws Exception {
        int ops = 20000;
        boolean time = true;
        for (String a : args) {
            if (a.equals("facts")) time = false;
            else ops = Integer.parseInt(a);
        }
        System.out.println("fact prealloc-origin " + describe(PRE, "<clinit>"));
        for (int row = 0; row < ROWS.length; row++) {
            factsAndTime(row, ops, time);
        }
        threadsRow(ops, time);
        // `sink` is written by the worker threads too (racy on purpose: it only keeps the loops
        // alive), so it is not a fact.
        System.out.println("fact done");
    }
}
