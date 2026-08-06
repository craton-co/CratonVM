// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Isolate what `mock()`-ing a `ThreadLocal` subclass does to `ThreadLocal`
// itself.
//
// Mockito's inline mock maker instruments the target's whole superclass chain,
// so `mock(NamedThreadLocal.class)` retransforms `java.lang.ThreadLocal`
// (confirmed with CRATONVM_DBG=retransform: java/lang/Object,
// java/lang/ThreadLocal, then the target). Every question below is asked once
// before that retransform and once after, so a failure names which invariant
// the retransform broke rather than "Mockito stopped working".
//
// Every ThreadLocal is constructed UP FRONT: after the retransform,
// `ThreadLocal.<init>` runs Mockito's woven constructor advice, and if that
// path is broken then merely constructing one throws — which would mask the
// state questions this probe exists to ask.
//
// Each check is individually guarded so one failure does not hide the rest.

import java.util.concurrent.Callable;

public class ThreadLocalRetransformProbe {

    static final ThreadLocal<String> SET_BEFORE = new ThreadLocal<>();
    static final ThreadLocal<String> SET_AFTER = new ThreadLocal<>();
    static final ThreadLocal<Boolean> WITH_INITIAL = new ThreadLocal<Boolean>() {
        @Override
        protected Boolean initialValue() {
            return Boolean.TRUE;
        }
    };
    static final ThreadLocal<Boolean> SUPPLIED = ThreadLocal.withInitial(() -> Boolean.TRUE);

    static int failures = 0;

    static void check(String what, Callable<Object> actual, Object expected) {
        Object got;
        try {
            got = actual.call();
        } catch (Throwable t) {
            failures++;
            System.out.println("FAIL " + what + ": threw " + t);
            return;
        }
        boolean ok = expected == null ? got == null : expected.equals(got);
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "ok   " : "FAIL ") + what + ": got " + got + ", expected " + expected);
    }

    public static void main(String[] args) {
        String target = args.length > 0 ? args[0] : "org.springframework.core.NamedThreadLocal";

        SET_BEFORE.set("hello");
        check("before/set-get", () -> SET_BEFORE.get(), "hello");
        check("before/initialValue-override", () -> WITH_INITIAL.get(), Boolean.TRUE);
        check("before/withInitial-supplier", () -> SUPPLIED.get(), Boolean.TRUE);
        check("before/construct-ThreadLocal", () -> new ThreadLocal<String>() != null, Boolean.TRUE);

        // --- retransform ThreadLocal by mocking a subclass of it ---
        Object mock = null;
        try {
            Class<?> cls = Class.forName(target);
            mock = Class.forName("org.mockito.Mockito")
                    .getMethod("mock", Class.class)
                    .invoke(null, cls);
        } catch (Throwable t) {
            failures++;
            Throwable c = t.getCause() != null ? t.getCause() : t;
            System.out.println("FAIL mock(" + target + ") threw " + c);
        }
        System.out.println("mocked " + target + " -> " + (mock == null ? "null" : "ok"));

        // --- the same questions, after ---
        // The value survives only if `set` and `get` agree about where it lives.
        check("after/value-set-before-survives", () -> SET_BEFORE.get(), "hello");
        check("after/set-get-round-trip", () -> {
            SET_AFTER.set("world");
            return SET_AFTER.get();
        }, "world");
        check("after/initialValue-override", () -> WITH_INITIAL.get(), Boolean.TRUE);
        check("after/withInitial-supplier", () -> SUPPLIED.get(), Boolean.TRUE);
        check("after/construct-ThreadLocal", () -> new ThreadLocal<String>() != null, Boolean.TRUE);

        System.out.println(failures == 0 ? "PROBE-OK" : "PROBE-FAIL " + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }
}
