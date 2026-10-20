// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L2 -- page
// i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range
// (the w102e flake of `test_callee_that_cannot_catch_propagates_instead_of_re_running`,
// 19997 of 20000).
//
// `step` latches, then calls `finish`, which throws, from OUTSIDE its own
// `try (RuntimeException)`. HotSpot: every call throws out of `step`, and the
// prefix runs exactly once per call.
//
// The race this stresses: an interpreter door checks that `step`'s compiled
// body is entrant, the background tier-up retires it (C1 -> C2 supersede)
// before the call, and the call runs the not-entrant stub. The stub's first
// pass answers the call through the VM, where the exception already left
// `step`, and re-posts it with no throw-site stamp. The door then read "pc
// unknown" and the pc-unknown search ran `step`'s `catch (RuntimeException)`
// on exception class alone: `step` returned -2. The fix carries the stub's
// escape record out of the drain (`DrainedJitSignals::escaped_a_redispatch_of`).
//
// The race needs a supersede to land between two instructions, so a clean
// run proves little; a dirty one (any non-zero `swallowed` / `rerun`) is the
// defect. The `reflect` and `handle` rows use a FRESH copy of the shape per
// round (the class bytes defined again by a new loader), so every round has
// its own tier-up to race against; `direct` calls the application copy by
// name. The callers are reached three ways: a method reference, through
// `Method.invoke` (a VM invoke, i.e. the interpreter's method-entry door), and
// through `MethodHandle.invokeExact`.
// Run by default, under load (several copies at once), with `--nojit`, and
// with `CRATONVM_JIT_NOT_ENTRANT_ESCAPE=0` (the old drain; may show the
// defect under load).
//
// Expected (HotSpot 25), every line identical:
//   direct rounds=12 calls=240000 caught=240000 swallowed=0 rerun=0
//   reflect rounds=12 calls=240000 caught=240000 swallowed=0 rerun=0
//   handle rounds=12 calls=240000 caught=240000 swallowed=0 rerun=0
import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L2W103RetiredBodyEscape {
    static final int ROUNDS = 12;
    static final int CALLS = 20_000;

    /** The shape: copied per round by a fresh loader. */
    public static class Shape {
        public static boolean latched;
        public static int prefixRuns;

        static int work(int x) {
            return x + 1;
        }

        static void finish() {
            throw new IllegalStateException("finish");
        }

        public static int step(int x) {
            if (latched) {
                return -1;
            }
            latched = true;
            prefixRuns++;
            finish();
            try {
                return work(x);
            } catch (RuntimeException e) {
                return -2;
            }
        }
    }

    static final byte[] SHAPE_BYTES = shapeBytes();

    static byte[] shapeBytes() {
        try (InputStream in = L2W103RetiredBodyEscape.class.getResourceAsStream(
                "L2W103RetiredBodyEscape$Shape.class")) {
            return in.readAllBytes();
        } catch (IOException e) {
            throw new UncheckedIOException(e);
        }
    }

    /** A loader that defines its own copy of `Shape` (and delegates the rest). */
    static final class Fresh extends ClassLoader {
        Fresh() {
            super(L2W103RetiredBodyEscape.class.getClassLoader());
        }

        Class<?> shape() {
            return defineClass(Shape.class.getName(), SHAPE_BYTES, 0, SHAPE_BYTES.length);
        }
    }

    static final class Tally {
        long calls;
        long caught;
        long swallowed;
        long rerun;

        void line(String kind) {
            System.out.println(kind + " rounds=" + ROUNDS + " calls=" + calls + " caught=" + caught
                    + " swallowed=" + swallowed + " rerun=" + rerun);
        }
    }

    interface Caller {
        int call(int x) throws Throwable;
    }

    static void round(Class<?> c, Caller caller, Tally t) throws Throwable {
        java.lang.reflect.Field latched = c.getField("latched");
        java.lang.reflect.Field prefixRuns = c.getField("prefixRuns");
        for (int i = 0; i < CALLS; i++) {
            latched.setBoolean(null, false);
            prefixRuns.setInt(null, 0);
            t.calls++;
            try {
                caller.call(i);
                t.swallowed++;
            } catch (IllegalStateException e) {
                t.caught++;
            } catch (InvocationTargetException e) {
                if (e.getCause() instanceof IllegalStateException) {
                    t.caught++;
                } else {
                    throw e;
                }
            }
            if (prefixRuns.getInt(null) != 1) {
                t.rerun++;
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        Tally direct = new Tally();
        Tally reflect = new Tally();
        Tally handle = new Tally();
        for (int r = 0; r < ROUNDS; r++) {
            // The application copy, called by name: its tier-up happens in the
            // first round, while this frame is still interpreted.
            round(Shape.class, Shape::step, direct);
            // Fresh copies: each has its own counters and its own tier-up.
            Class<?> c2 = new Fresh().shape();
            Method m2 = c2.getMethod("step", int.class);
            round(c2, x -> (Integer) m2.invoke(null, x), reflect);
            Class<?> c3 = new Fresh().shape();
            MethodHandle mh3 = MethodHandles.publicLookup()
                    .findStatic(c3, "step", MethodType.methodType(int.class, int.class));
            round(c3, x -> (int) mh3.invokeExact(x), handle);
        }
        direct.line("direct");
        reflect.line("reflect");
        handle.line("handle");
    }
}
