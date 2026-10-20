// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L2 (hunt): exception paths of compiled
// code against the interpreter, cold (first call, interpreted) vs hot (after
// the method and its small callees are warmed past every JIT threshold, so the
// leaves are inlined into their callers):
//
//   * the stack trace of an exception raised INSIDE an inlined leaf (implicit
//     NPE, `/ by zero`, AIOOBE, CCE, explicit `athrow`): method names and line
//     numbers of every frame of this class, innermost first;
//   * JEP 358 helpful NPE texts computed for a trap inside an inlined leaf
//     (parameters, static fields, method return values, unboxing, array
//     store, monitorenter, invokeinterface);
//   * ArithmeticException from irem / ldiv / lrem and from a divisor the
//     bytecode names as the constant 0;
//   * a handler of the compiled method itself entered for the first time from
//     compiled code (the warm-up never threw), with long / double / reference
//     locals live across the try: the handler must see their values;
//   * monitors released when an exception leaves a compiled synchronized
//     method or block;
//   * OSR: a long-running loop with a handler inside it, a loop whose
//     exception escapes (no iteration may run twice), and a loop enclosed by a
//     handler of the same method.
//
// Each warm/cold row prints `name cold=<answer> hot=<answer>`; HotSpot 25
// prints cold == hot on every row and these exact texts. The OSR rows print
// fixed values. Diff the whole output against `java`. Run by default, with
// `--nojit`, with `CRATONVM_BG_COMPILE=0`, in both `--jdk-only` and
// `--compatible`. Compile with plain `javac` (no `-g`: line numbers yes,
// local-variable names no, so JEP 358 says `<localN>` / `<parameterN>` on
// both VMs).
//
// Wave 104 hand-back: HotSpot C2's `OmitStackTraceInFastThrow` (on by
// default) answers a hot implicit exception with a preallocated one -- null
// message, EMPTY stack trace -- once the site has trapped before; whether it
// does is compile-history dependent. That is not behaviour to copy, so the
// probe relaunches itself as a child with `-XX:-OmitStackTraceInFastThrow`
// (the same executable and arguments, from `ProcessHandle.current().info()`;
// `cratonvm` accepts the flag, `VmConfig::omit_stack_trace_in_fast_throw`)
// and passes the child's stdout through unchanged. When the arguments are not
// available (some platforms) it runs in-process, which is exact on cratonvm
// (it never fast-throws by default) and may show fast-throw rows on HotSpot.
public class L2W104CompiledExceptionPaths {
    static final int WARM = 30_000;

    static volatile int ZERO = 0;
    static volatile long LZERO = 0L;
    static Holder NULL_HOLDER = null;
    static int[] NULL_INTS = null;
    static final Object LOCK = new Object();

    static class Holder {
        int x;
        long wide;
        Holder next;
    }

    interface Shape {
        Object run(int mode) throws Throwable;
    }

    interface Sized {
        int size();
    }

    static String frames(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : t.getStackTrace()) {
            if (!e.getClassName().equals(L2W104CompiledExceptionPaths.class.getName())) {
                continue;
            }
            String m = e.getMethodName();
            if (m.startsWith("leaf") || m.startsWith("mid") || m.startsWith("top")) {
                sb.append(' ').append(m).append(':').append(e.getLineNumber());
            }
        }
        return sb.toString();
    }

    static String answer(Shape s, int mode) {
        try {
            return String.valueOf(s.run(mode));
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage() + " @" + frames(t);
        }
    }

    static void row(String name, Shape s) {
        String cold = answer(s, 1);
        for (int i = 0; i < WARM; i++) {
            answer(s, 0);
        }
        String hot = answer(s, 1);
        System.out.println(name + " cold=" + cold + " hot=" + hot);
    }

    // ---- leaves (small, so the optimizing tier inlines them) ----
    static int leafGet(Holder h) {
        return h.x;
    }

    static int leafDiv(int a, int b) {
        return a / b;
    }

    static int leafRem(int a, int b) {
        return a % b;
    }

    static long leafLdiv(long a, long b) {
        return a / b;
    }

    static long leafLrem(long a, long b) {
        return a % b;
    }

    static int leafIdx(int[] a, int i) {
        return a[i];
    }

    static String leafCast(Object o) {
        return (String) o;
    }

    static int leafThrow(int v) {
        if (v < 0) {
            throw new IllegalStateException("negative " + v);
        }
        return v;
    }

    static int leafConstDiv(int a, int mode) {
        return mode == 1 ? a / 0 : a;
    }

    static long leafConstLrem(long a, int mode) {
        return mode == 1 ? a % 0L : a;
    }

    static Holder leafHolder(int mode) {
        return mode == 1 ? null : new Holder();
    }

    static int leafReturnValue(int mode) {
        return leafHolder(mode).x;
    }

    static int leafStatic(int mode) {
        Holder h = mode == 1 ? NULL_HOLDER : new Holder();
        return h.x;
    }

    static int leafStaticDirect(int mode) {
        return mode == 1 ? NULL_HOLDER.x : 0;
    }

    static int leafUnbox(Integer i) {
        return i;
    }

    static void leafStore(int[] a, int v) {
        a[0] = v;
    }

    static void leafWideStore(Holder h, long v) {
        h.wide = v;
    }

    static int leafChain(Holder h) {
        return h.next.next.x;
    }

    static int leafIface(Sized s) {
        return s.size();
    }

    static int leafSync(Object o) {
        synchronized (o) {
            return 1;
        }
    }

    static int leafLength(int[] a) {
        return a.length;
    }

    // ---- two-level callers ----
    static int midGet(Holder h) {
        return leafGet(h) + 1;
    }

    static int midDiv(int a, int b) {
        return leafDiv(a, b) + 1;
    }

    static int midIdx(int[] a, int i) {
        return leafIdx(a, i) + 1;
    }

    static int midThrow(int v) {
        return leafThrow(v) + 1;
    }

    static int topGet(Holder h) {
        return midGet(h) * 2;
    }

    static int topThrow(int v) {
        return midThrow(v) * 2;
    }

    // ---- a handler first entered from compiled code ----
    static Object topHandlerLocals(int mode) {
        long l = mode * 3L + 7L;
        double d = mode * 0.5 + 0.25;
        String s = "s" + mode;
        int[] a = new int[2];
        int r;
        try {
            r = a[mode == 1 ? 5 : 0] + 1;
        } catch (ArrayIndexOutOfBoundsException e) {
            return "caught l=" + l + " d=" + d + " s=" + s + " msg=" + e.getMessage() + " @" + frames(e);
        }
        return "r=" + r + " l=" + l + " d=" + d + " s=" + s;
    }

    static Object topHandlerOfInlinedThrow(int mode) {
        long before = mode * 11L;
        try {
            return midThrow(mode == 1 ? -4 : 4);
        } catch (IllegalStateException e) {
            return "caught before=" + before + " msg=" + e.getMessage() + " @" + frames(e);
        }
    }

    static Object topHandlerOfInlinedDiv(int mode) {
        int k = mode + 40;
        try {
            return midDiv(k, mode == 1 ? ZERO : 2);
        } catch (ArithmeticException e) {
            return "caught k=" + k + " msg=" + e.getMessage() + " @" + frames(e);
        }
    }

    static Object topHandlerRethrow(int mode) {
        try {
            return midGet(mode == 1 ? null : new Holder());
        } catch (NullPointerException e) {
            throw new IllegalArgumentException("wrapped", e);
        }
    }

    // ---- monitors released on an exceptional exit ----
    static synchronized int topSyncMethod(int mode) {
        return leafDiv(10, mode == 1 ? ZERO : 5);
    }

    static Object topSyncMethodRow(int mode) {
        String r;
        try {
            r = String.valueOf(topSyncMethod(mode));
        } catch (ArithmeticException e) {
            r = "AE";
        }
        return r + " holdsClass=" + Thread.holdsLock(L2W104CompiledExceptionPaths.class);
    }

    static int topSyncBlock(int mode) {
        synchronized (LOCK) {
            return leafIdx(new int[1], mode == 1 ? 1 : 0);
        }
    }

    static Object topSyncBlockRow(int mode) {
        String r;
        try {
            r = String.valueOf(topSyncBlock(mode));
        } catch (ArrayIndexOutOfBoundsException e) {
            r = "AIOOBE";
        }
        return r + " holdsLock=" + Thread.holdsLock(LOCK);
    }

    // ---- OSR ----
    static String osrHandlerInLoop(int n) {
        long sum = 0;
        int caught = 0;
        String lastMsg = "";
        int lastLine = -1;
        for (int i = 0; i < n; i++) {
            try {
                sum += 10 / (i % 1000 == 999 ? ZERO : 1);
            } catch (ArithmeticException e) {
                caught++;
                lastMsg = e.getMessage();
                StackTraceElement[] st = e.getStackTrace();
                lastLine = st.length == 0 ? -1 : st[0].getLineNumber();
            }
        }
        return "sum=" + sum + " caught=" + caught + " msg=" + lastMsg + " line=" + lastLine;
    }

    static int osrSideEffects;

    static void osrEscape(int n, int throwAt) {
        for (int i = 0; i < n; i++) {
            osrSideEffects++;
            if (i == throwAt) {
                leafThrow(-i);
            }
        }
    }

    static String osrEscapeRow() {
        osrSideEffects = 0;
        try {
            osrEscape(400_000, 300_000);
            return "no throw";
        } catch (IllegalStateException e) {
            return "effects=" + osrSideEffects + " msg=" + e.getMessage() + " @" + frames(e);
        }
    }

    static String osrEnclosedByHandler(int n) {
        int done = 0;
        long acc = 0;
        try {
            for (int i = 0; i < n; i++) {
                acc += leafIdx(new int[4], i == n - 3 ? 4 : i & 3);
                done++;
            }
            return "completed";
        } catch (ArrayIndexOutOfBoundsException e) {
            return "done=" + done + " acc=" + acc + " msg=" + e.getMessage() + " @" + frames(e);
        }
    }

    static String osrNpeInLoop(int n) {
        int caught = 0;
        int last = -1;
        Holder live = new Holder();
        live.x = 1;
        long sum = 0;
        for (int i = 0; i < n; i++) {
            try {
                sum += leafGet(i % 5000 == 4999 ? null : live);
            } catch (NullPointerException e) {
                caught++;
                last = i;
            }
        }
        return "sum=" + sum + " caught=" + caught + " last=" + last;
    }

    public static void main(String[] args) throws Exception {
        if (System.getProperty("l2w104.child") == null) {
            java.util.Optional<String> cmd = ProcessHandle.current().info().command();
            java.util.Optional<String[]> argv = ProcessHandle.current().info().arguments();
            // On Windows, ProcessHandle.Info.arguments() is empty for HotSpot's
            // java.exe, so rebuild the plain launch from the class path there.
            // Only for a launcher named java: any other launcher (a VM with mode
            // flags of its own) must pass its real arguments or run in-process.
            if (cmd.isPresent() && (argv.isEmpty() || argv.get().length == 0)) {
                String exe = java.nio.file.Path.of(cmd.get()).getFileName().toString();
                if (exe.equals("java") || exe.equals("java.exe")) {
                    argv = java.util.Optional.of(new String[] {
                        "-cp", System.getProperty("java.class.path"), "L2W104CompiledExceptionPaths"});
                }
            }
            if (cmd.isPresent() && argv.isPresent() && argv.get().length > 0) {
                java.util.List<String> c = new java.util.ArrayList<>();
                c.add(cmd.get());
                c.add("-XX:-OmitStackTraceInFastThrow");
                c.add("-Dl2w104.child=1");
                c.addAll(java.util.Arrays.asList(argv.get()));
                Process p = new ProcessBuilder(c)
                        .redirectError(ProcessBuilder.Redirect.INHERIT)
                        .start();
                p.getOutputStream().close();
                p.getInputStream().transferTo(System.out);
                int code = p.waitFor();
                System.out.flush();
                if (code != 0) {
                    System.exit(code);
                }
                return;
            }
        }
        rows();
    }

    static void rows() {
        // stack traces from inlined leaves
        row("trace-npe-two-levels", m -> topGet(m == 1 ? null : new Holder()));
        row("trace-div-one-level", m -> midDiv(7, m == 1 ? ZERO : 7));
        row("trace-aioobe-one-level", m -> midIdx(new int[3], m == 1 ? 3 : 2));
        row("trace-athrow-two-levels", m -> topThrow(m == 1 ? -9 : 9));
        row("trace-cce-leaf", m -> leafCast(m == 1 ? (Object) Integer.valueOf(3) : "s"));
        // helpful NPE texts from inlined leaves
        row("npe-parameter-field", m -> leafGet(m == 1 ? null : new Holder()));
        row("npe-return-value", m -> leafReturnValue(m));
        row("npe-static-field-via-local", m -> leafStatic(m));
        row("npe-static-field-direct", m -> leafStaticDirect(m));
        row("npe-unboxing", m -> leafUnbox(m == 1 ? null : Integer.valueOf(5)));
        row("npe-array-store", m -> {
            leafStore(m == 1 ? NULL_INTS : new int[1], 4);
            return "ok";
        });
        row("npe-long-field-store", m -> {
            leafWideStore(m == 1 ? null : new Holder(), 9L);
            return "ok";
        });
        row("npe-field-chain", m -> {
            Holder h = new Holder();
            h.next = new Holder();
            if (m == 0) {
                h.next.next = new Holder();
            }
            return leafChain(h);
        });
        row("npe-invokeinterface", m -> leafIface(m == 1 ? null : () -> 3));
        row("npe-monitorenter", m -> leafSync(m == 1 ? null : LOCK));
        row("npe-arraylength", m -> leafLength(m == 1 ? null : new int[2]));
        // arithmetic
        row("irem-zero", m -> leafRem(17, m == 1 ? ZERO : 5));
        row("ldiv-zero", m -> leafLdiv(17L, m == 1 ? LZERO : 5L));
        row("lrem-zero", m -> leafLrem(17L, m == 1 ? LZERO : 5L));
        row("idiv-const-zero", m -> leafConstDiv(8, m));
        row("lrem-const-zero", m -> leafConstLrem(8L, m));
        row("irem-min-by-minus-one", m -> leafRem(Integer.MIN_VALUE, m == 1 ? -1 : 3));
        row("ldiv-min-by-minus-one", m -> leafLdiv(Long.MIN_VALUE, m == 1 ? -1L : 3L));
        // handlers first entered from compiled code
        row("handler-locals-live", L2W104CompiledExceptionPaths::topHandlerLocals);
        row("handler-of-inlined-athrow", L2W104CompiledExceptionPaths::topHandlerOfInlinedThrow);
        row("handler-of-inlined-div", L2W104CompiledExceptionPaths::topHandlerOfInlinedDiv);
        row("handler-rethrow-wrapped", m -> {
            try {
                return topHandlerRethrow(m);
            } catch (IllegalArgumentException e) {
                Throwable c = e.getCause();
                return e.getMessage() + " cause=" + c.getClass().getName() + ": " + c.getMessage()
                        + " @" + frames(c);
            }
        });
        // monitors
        row("sync-method-exit", L2W104CompiledExceptionPaths::topSyncMethodRow);
        row("sync-block-exit", L2W104CompiledExceptionPaths::topSyncBlockRow);
        // OSR (single long invocations)
        System.out.println("osr-handler-in-loop " + osrHandlerInLoop(300_000));
        System.out.println("osr-escape " + osrEscapeRow());
        System.out.println("osr-enclosed-by-handler " + osrEnclosedByHandler(300_000));
        System.out.println("osr-npe-in-loop " + osrNpeInLoop(300_000));
        // and once more, now that every method above is compiled
        System.out.println("osr-handler-in-loop-again " + osrHandlerInLoop(300_000));
        System.out.println("osr-escape-again " + osrEscapeRow());
        System.out.println("osr-enclosed-by-handler-again " + osrEnclosedByHandler(300_000));
    }
}
