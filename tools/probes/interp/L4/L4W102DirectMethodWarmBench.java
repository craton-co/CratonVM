// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 102, lane L4: bench and behaviour probe for
// `CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER` (default off; page
// `docs/internal/fixed-bugs/interpreter-i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-FIXED-20261018.md`,
// "Wave 102").
//
// A warm `findStatic` / `findVirtual` handle (a plain `DirectMethodHandle`)
// called by `invokeExact` / `invoke` ran the JDK invoker linker and the
// handle's whole `LambdaForm` chain, interpreted, on every call. With the
// switch on, a site whose `MethodType` IS the handle's type calls the member
// directly when nothing the chain would run can be seen.
//
// Behaviour lines (both modes, switch on and off, JIT and --nojit, must equal
// HotSpot): the lane's hazards -- overriding dispatch, a null receiver after
// warm calls, the first call of an uninitialized holder (its <clinit> runs
// once), a throw from the target (trace frames of this probe's classes, hidden
// frames filtered as HotSpot filters them), the caller seen by
// StackWalker.getCallerClass inside the target, another site type
// (WrongMethodTypeException), a handle from a second caller class, a
// collection between calls, a private method (findVirtual -> $Special,
// never served) and an interface method (never served).
//
// Timing lines start with "TIME " (filter them before diffing):
//   TIME <row> med <ns> ns/op      (median of reps, 20000 calls per rep)
//
// Run: javac -d . L4W102DirectMethodWarmBench.java
//      java -cp . L4W102DirectMethodWarmBench                                     (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W102DirectMethodWarmBench [reps]
//      CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER=1 cratonvm --jdk-only [--nojit] -cp . L4W102DirectMethodWarmBench [reps]
//      (the same with --compatible; interleave the arms, take medians;
//       CRATONVM_DBG=mh-finder-census shows `lane direct-before-linker`)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources; TIME lines removed)
// static-exact 8
// virtual-exact 7 70
// virtual-invoke 7
// null-receiver java.lang.NullPointerException
// lazy-init clinit-ran 1 answer 41 clinit-count 1
// throw java.lang.IllegalStateException boom [thrower, throwCall, main]
// caller-class L4W102DirectMethodWarmBench
// wrong-type java.lang.invoke.WrongMethodTypeException
// two-callers 8 8
// after-gc 8 8
// private 11
// interface 12
// jdk-site 5
// checksum static-warm-exact 160000
// checksum static-warm-invoke 160000
// checksum virtual-warm-exact 140000
// checksum virtual-warm-override 1400000
// checksum jdk-site-warm-exact 100000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

public class L4W102DirectMethodWarmBench {
    public static class Box {
        public int v = 7;

        public int get() {
            return v;
        }

        private int secret() {
            return 11;
        }

        static MethodHandle secretHandle() throws Throwable {
            return MethodHandles.lookup().findVirtual(Box.class, "secret", MethodType.methodType(int.class));
        }
    }

    public static class Big extends Box {
        @Override
        public int get() {
            return 70;
        }
    }

    public interface Shape {
        int sides();
    }

    public static class Sq implements Shape {
        public int sides() {
            return 12;
        }
    }

    public static class Lazy {
        static int clinitCount;

        static {
            clinitCount++;
            ran = 1;
        }

        static int ran;

        public static int answer() {
            return 41;
        }
    }

    public static int twice(int x) {
        return x * 2;
    }

    public static int thrower(int x) {
        throw new IllegalStateException("boom");
    }

    public static String callerClass() {
        return StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .getCallerClass().getName();
    }

    /** A second caller class: its own sites, the same handles. */
    static final class Other {
        static int call(MethodHandle twice, int x) throws Throwable {
            return (int) twice.invokeExact(x);
        }
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static final int CALLS = 20000;

    interface Row {
        long run() throws Throwable;
    }

    static void bench(String row, int reps, Row r) throws Throwable {
        long[] t = new long[reps];
        long sum0 = 0;
        for (int i = 0; i < reps; i++) {
            long t0 = System.nanoTime();
            long sum = r.run();
            t[i] = (System.nanoTime() - t0) / CALLS;
            if (i == 0) {
                sum0 = sum;
            }
        }
        System.out.printf(Locale.ROOT, "TIME %s med %.1f ns/op%n", row, median(t));
        System.out.println("checksum " + row + " " + sum0);
    }

    static int throwCall(MethodHandle h) throws Throwable {
        return (int) h.invokeExact(1);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType ii = MethodType.methodType(int.class, int.class);
        MethodHandle twice = l.findStatic(L4W102DirectMethodWarmBench.class, "twice", ii);
        MethodHandle get = l.findVirtual(Box.class, "get", MethodType.methodType(int.class));
        Box box = new Box();
        Box big = new Big();

        // Warm the sites first, then the behaviour rows.
        for (int i = 0; i < 300; i++) {
            int a = (int) twice.invokeExact(4);
            int b = (int) get.invokeExact(box);
        }
        System.out.println("static-exact " + (int) twice.invokeExact(4));
        System.out.println("virtual-exact " + (int) get.invokeExact(box) + " " + (int) get.invokeExact(big));
        System.out.println("virtual-invoke " + (int) get.invoke(box));
        try {
            int x = (int) get.invokeExact((Box) null);
            System.out.println("null-receiver no exception " + x);
        } catch (Throwable e) {
            System.out.println("null-receiver " + e.getClass().getName());
        }
        MethodHandle lazy = l.findStatic(Lazy.class, "answer", MethodType.methodType(int.class));
        int lazyAnswer = (int) lazy.invokeExact();
        for (int i = 0; i < 300; i++) {
            lazyAnswer = (int) lazy.invokeExact();
        }
        System.out.println("lazy-init clinit-ran " + Lazy.ran + " answer " + lazyAnswer
                + " clinit-count " + Lazy.clinitCount);
        MethodHandle thr = l.findStatic(L4W102DirectMethodWarmBench.class, "thrower", ii);
        for (int i = 0; i < 3; i++) {
            try {
                throwCall(thr);
            } catch (IllegalStateException e) {
                if (i == 2) {
                    List<String> frames = new ArrayList<>();
                    for (StackTraceElement f : e.getStackTrace()) {
                        if (f.getClassName().startsWith("L4W102")) {
                            frames.add(f.getMethodName());
                        }
                    }
                    System.out.println("throw " + e.getClass().getName() + " " + e.getMessage() + " " + frames);
                }
            }
        }
        MethodHandle cc = l.findStatic(L4W102DirectMethodWarmBench.class, "callerClass",
                MethodType.methodType(String.class));
        String seen = null;
        for (int i = 0; i < 300; i++) {
            seen = (String) cc.invokeExact();
        }
        System.out.println("caller-class " + seen);
        try {
            long w = (long) twice.invokeExact(4);
            System.out.println("wrong-type no exception " + w);
        } catch (Throwable e) {
            System.out.println("wrong-type " + e.getClass().getName());
        }
        System.out.println("two-callers " + Other.call(twice, 4) + " " + (int) twice.invokeExact(4));
        int beforeGc = (int) twice.invokeExact(4);
        System.gc();
        System.out.println("after-gc " + beforeGc + " " + (int) twice.invokeExact(4));
        MethodHandle secret = Box.secretHandle();
        int s = 0;
        for (int i = 0; i < 300; i++) {
            s = (int) secret.invokeExact(box);
        }
        System.out.println("private " + s);
        MethodHandle sides = l.findVirtual(Shape.class, "sides", MethodType.methodType(int.class));
        Shape sq = new Sq();
        int n = 0;
        for (int i = 0; i < 300; i++) {
            n = (int) sides.invokeExact(sq);
        }
        System.out.println("interface " + n);
        MethodHandle len = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
        System.out.println("jdk-site " + (int) len.invokeExact("hello"));

        bench("static-warm-exact", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) twice.invokeExact(4);
            }
            return sum;
        });
        bench("static-warm-invoke", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) twice.invoke(4);
            }
            return sum;
        });
        bench("virtual-warm-exact", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) get.invokeExact(box);
            }
            return sum;
        });
        bench("virtual-warm-override", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) get.invokeExact(big);
            }
            return sum;
        });
        bench("jdk-site-warm-exact", reps, () -> {
            long sum = 0;
            for (int i = 0; i < CALLS; i++) {
                sum += (int) len.invokeExact("hello");
            }
            return sum;
        });
    }
}
