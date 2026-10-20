// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L3 -- the per-call cost of an inherited
// static called through a subclass that is never initialized (HotSpot
// initializes only the declaring class, JVMS 5.5), with and without
// CRATONVM_STATIC_OWNER_SUPERS_INITIALIZED (the by-name routes answer
// "nothing to initialize" from one superclass walk instead of a member
// resolution per call; proposal
// i71-L3-proposal-an-inherited-static-owner-answers-from-its-superclasses-bits).
//
// Rows (ITERS calls each, after one untimed warm-up pass):
//   direct-inherited   `Sub.helper(i)`: invokestatic through the subclass
//   direct-declared    `Base.helper(i)`: the control (owner initialized)
//   reflect-inherited  Sub.class.getMethod("helper") -> Method.invoke
//   handle-inherited   findStatic(Sub.class, "helper").invokeExact
//   handle-deep        the same through a subclass three levels down
//   lambda-inherited   the method reference `Sub::helper`
//
// Every row runs REPS timed repetitions and prints the MINIMUM. Stdout
// carries a deterministic checksum line per row (no "ns/") and a timing line
// per row ("<row> <x> ns/op").
//
// A/B: the SAME binary, interleaved, several rounds, medians of the
// per-round minima, `--compatible` and `--jdk-only`, JIT and `--nojit`:
//   (a) CRATONVM_STATIC_OWNER_SUPERS_INITIALIZED unset
//   (b) CRATONVM_STATIC_OWNER_SUPERS_INITIALIZED=1
// (b) should be at or under (a) on the *-inherited / handle-deep rows that
// reach a by-name route per call; direct-declared is the control. A row
// that does not move under (b) does not reach `static_owner_init_pending`
// per call on that mode (the page's "Progress (wave 72)" asks which).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-inherited checksum=20000300000
// direct-declared checksum=20000300000
// reflect-inherited checksum=20000300000
// handle-inherited checksum=20000300000
// handle-deep checksum=20000300000
// lambda-inherited checksum=20000300000
// subs-initialized []
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.function.IntUnaryOperator;

public class L3W72InheritedStaticBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;
    static final StringBuffer LOG = new StringBuffer();

    public static class Base {
        public static int helper(int x) { return x + 1; }
    }
    public static class Sub extends Base {
        static { LOG.append("Sub;"); }
    }
    public static class Mid1 extends Base {
        static { LOG.append("Mid1;"); }
    }
    public static class Mid2 extends Mid1 {
        static { LOG.append("Mid2;"); }
    }
    public static class Deep extends Mid2 {
        static { LOG.append("Deep;"); }
    }

    interface Body {
        int run(int i) throws Throwable;
    }

    static void bench(String name, Body body) throws Throwable {
        long checksum = 0;
        for (int i = 0; i < ITERS; i++) {
            checksum += body.run(i + 1);
        }
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long sum = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < ITERS; i++) {
                sum += body.run(i + 1);
            }
            long t1 = System.nanoTime();
            best = Math.min(best, t1 - t0);
            if (sum != checksum) {
                System.out.println(name + " unstable " + sum + " vs " + checksum);
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%s %.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    static int directInherited(int i) { return Sub.helper(i); }
    static int directDeclared(int i) { return Base.helper(i); }

    public static void main(String[] args) throws Throwable {
        Method reflect = Sub.class.getMethod("helper", int.class);
        MethodType type = MethodType.methodType(int.class, int.class);
        MethodHandle handle = MethodHandles.lookup().findStatic(Sub.class, "helper", type);
        MethodHandle deep = MethodHandles.lookup().findStatic(Deep.class, "helper", type);
        IntUnaryOperator op = Sub::helper;

        // checksum = sum of (i + 2) for i in [0, ITERS) = ITERS(ITERS-1)/2
        // + 2 ITERS = 19999900000 + 400000 = 20000300000.
        bench("direct-inherited", L3W72InheritedStaticBench::directInherited);
        bench("direct-declared", L3W72InheritedStaticBench::directDeclared);
        bench("reflect-inherited", i -> (Integer) reflect.invoke(null, i));
        bench("handle-inherited", i -> (int) handle.invokeExact(i));
        bench("handle-deep", i -> (int) deep.invokeExact(i));
        bench("lambda-inherited", i -> op.applyAsInt(i));
        System.out.println("subs-initialized [" + LOG + "]");
    }
}
