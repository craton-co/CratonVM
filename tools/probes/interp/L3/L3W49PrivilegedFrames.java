// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L3 -- the fix of
// docs/internal/fixed-bugs/interpreter-L3-a-served-doprivileged-lists-no-accesscontroller-frame-FIXED-20261013.md
// -- a `PrivilegedAction` run
// by `AccessController.doPrivileged` has the `AccessController.doPrivileged`
// frame below it in every view on HotSpot. This VM serves the three
// `doPrivileged` overloads it registers with a bridge that runs the action
// itself; since wave 49 the bridge records its entry and every capture lists
// the frame (`CRATONVM_STANDIN_PRIVILEGED_FRAMES`).
//
// Rows print the frames from the action's body down to (not including)
// `main`, as `Class.method@bci` for walks and `Class.method:line` for traces.
// The first row runs on a cold VM and is repeated as the last row.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// cold-action-trace: P.lambda$main$0:100 < java.security.AccessController.doPrivileged:74
// action-walk: P.lambda$main$1@0 < java.security.AccessController.doPrivileged@1
// exception-action-walk: P.lambda$main$2@0 < java.security.AccessController.doPrivileged@1
// context-walk: P.lambda$main$3@0 < java.security.AccessController.doPrivileged@1
// checked-wrapped: java.security.PrivilegedActionException cause=java.io.IOException
// checked-trace: java.security.AccessController.wrapException:394 < java.security.AccessController.doPrivileged:256
// checked-cause-trace: P.lambda$main$4:109 < java.security.AccessController.doPrivileged:251
// unchecked-trace: P.lambda$main$5:118 < java.security.AccessController.doPrivileged:74
// nested-walk: P.lambda$main$7@0 < java.security.AccessController.doPrivileged@1 < P.lambda$main$6@5 < java.security.AccessController.doPrivileged@1
// get-stack-trace: java.lang.Thread.getStackTrace:2193 < P.lambda$main$8:126 < java.security.AccessController.doPrivileged:74
// reflect-inside: P.target:57 < jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104 < java.lang.reflect.Method.invoke:565 < P.lambda$main$9:129 < java.security.AccessController.doPrivileged:74
// combiner-walk: P.lambda$main$10@0 < java.security.AccessController.doPrivilegedWithCombiner@1
// caller-class: java.security.AccessController
// hot-trace: P.lambda$hot$0:64 < java.security.AccessController.doPrivileged:74 < P.hot:63
// warm-action-trace: P.lambda$main$12:136 < java.security.AccessController.doPrivileged:74
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 49 (both modes): the `doPrivileged` frame is missing
// from every row but `combiner-walk` (whose overload runs its real bytecode),
// and `checked-trace` starts at `main`. `CRATONVM_STANDIN_PRIVILEGED_FRAMES=0`
// restores that. Positive control: `CRATONVM_DBG_STTRACE=1` prints
// `STTRACE_DBG_REFLECT calls=N named=N` with N >= 1 for each capture inside an
// action, and `STTRACE_DBG_REFLECT raise ... relined=true` for `checked-trace`.
// Line numbers are the running image's (25.0.3 above); the host's HotSpot
// may differ.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W49PrivilegedFrames`.
import java.io.IOException;
import java.lang.reflect.Method;
import java.security.AccessController;
import java.security.PrivilegedAction;
import java.security.PrivilegedActionException;
import java.security.PrivilegedExceptionAction;
import java.util.Arrays;
import java.util.stream.Collectors;

@SuppressWarnings("removal")
public class L3W49PrivilegedFrames {
    static Throwable target() {
        return new Throwable();
    }

    static String hot() {
        String last = null;
        for (int i = 0; i < 20000; i++) {
            Throwable t = AccessController.doPrivileged(
                    (PrivilegedAction<Throwable>) () -> new Throwable());
            if (i == 19999) last = trace(t, "P.hot:");
        }
        return last;
    }

    static String name(String c) {
        return c.replace("L3W49PrivilegedFrames", "P");
    }

    static String walk() {
        return StackWalker.getInstance().walk(s -> s.skip(1)
                .map(f -> name(f.getClassName()) + "." + f.getMethodName() + "@" + f.getByteCodeIndex())
                .takeWhile(n -> !n.startsWith("P.main"))
                .collect(Collectors.joining(" < ")));
    }

    static String trace(Throwable t, String stop) {
        return trace(t.getStackTrace(), stop);
    }

    static String trace(StackTraceElement[] st, String stop) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            String n = name(e.getClassName()) + "." + e.getMethodName() + ":" + e.getLineNumber();
            if (sb.length() > 0) sb.append(" < ");
            sb.append(n);
            if (n.startsWith(stop)) break;
            if (e.getMethodName().equals("main")) break;
        }
        String s = sb.toString();
        int m = s.indexOf(" < P.main:");
        return m >= 0 ? s.substring(0, m) : s;
    }

    public static void main(String[] args) throws Exception {
        Throwable cold = AccessController.doPrivileged((PrivilegedAction<Throwable>) () -> new Throwable());
        System.out.println("cold-action-trace: " + trace(cold, "P.main:"));
        System.out.println("action-walk: " + AccessController.doPrivileged((PrivilegedAction<String>) () -> walk()));
        System.out.println("exception-action-walk: "
                + AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> walk()));
        System.out.println("context-walk: " + AccessController.doPrivileged(
                (PrivilegedAction<String>) () -> walk(), AccessController.getContext()));
        try {
            AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> {
                throw new IOException("checked");
            });
        } catch (PrivilegedActionException e) {
            System.out.println("checked-wrapped: " + e.getClass().getName() + " cause=" + e.getCause().getClass().getName());
            System.out.println("checked-trace: " + trace(e, "P.main:"));
            System.out.println("checked-cause-trace: " + trace(e.getCause(), "P.main:"));
        }
        try {
            AccessController.doPrivileged((PrivilegedAction<String>) () -> {
                throw new IllegalStateException("unchecked");
            });
        } catch (IllegalStateException e) {
            System.out.println("unchecked-trace: " + trace(e, "P.main:"));
        }
        System.out.println("nested-walk: " + AccessController.doPrivileged((PrivilegedAction<String>) () ->
                AccessController.doPrivileged((PrivilegedAction<String>) () -> walk())));
        System.out.println("get-stack-trace: " + AccessController.doPrivileged((PrivilegedAction<String>) () ->
                trace(Thread.currentThread().getStackTrace(), "P.main:")));
        Method m = L3W49PrivilegedFrames.class.getDeclaredMethod("target");
        System.out.println("reflect-inside: " + AccessController.doPrivileged((PrivilegedAction<String>) () -> {
            try { return trace((Throwable) m.invoke(null), "P.main:"); } catch (Exception e) { return e.toString(); }
        }));
        System.out.println("combiner-walk: "
                + AccessController.doPrivilegedWithCombiner((PrivilegedAction<String>) () -> walk()));
        System.out.println("caller-class: " + AccessController.doPrivileged((PrivilegedAction<String>) () ->
                StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).getCallerClass().getName()));
        System.out.println("hot-trace: " + hot());
        Throwable warm = AccessController.doPrivileged((PrivilegedAction<Throwable>) () -> new Throwable());
        System.out.println("warm-action-trace: " + trace(warm, "P.main:"));
    }
}
