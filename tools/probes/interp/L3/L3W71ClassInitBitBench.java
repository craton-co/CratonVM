// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L3 -- the per-call cost of the "is this
// class initialized" test on the native doors, with and without
// CRATONVM_CLASS_INIT_BIT_FIRST (the VM's initialized bitmap asked before the
// per-thread memo; proposal i59-L3-proposal-one-branch-class-initialized-test).
//
// Rows (ITERS calls each, after WARMUP):
//   reflect-static      Method.invoke of a static application method (the
//                       served door asks once per call whether the declaring
//                       class is initialized)
//   reflect-static-jdk  the same on a static JDK method (Integer.signum)
//   reflect-static-many Method.invoke round-robin over 8 classes' static
//                       methods (the memo's hash probe on 8 keys)
//   ctor-new-instance   Constructor.newInstance of an application class
//   field-get-static    Field.getInt of a static field
//   handle-static       MethodHandle.invokeExact of a static application
//                       method (the static door's class-init test)
//   getstatic-other     a loop reading another class's static (interpreter
//                       quickened under --nojit; the control: it should not
//                       move)
//
// Every row runs REPS timed repetitions after one untimed warm-up and prints
// the MINIMUM. Stdout carries a deterministic checksum line per row (no
// "ns/") and a timing line per row ("<row> <x> ns/op").
//
// A/B: the SAME binary, interleaved, several rounds, medians of the per-round
// minima, `--compatible` and `--jdk-only`, JIT and `--nojit`:
//   (a) CRATONVM_CLASS_INIT_BIT_FIRST unset   (b) CRATONVM_CLASS_INIT_BIT_FIRST=1
// (b) should be at or under (a) on the reflect / ctor / field / handle rows;
// getstatic-other is the control.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// reflect-static checksum=200000
// reflect-static-jdk checksum=200000
// reflect-static-many checksum=900000
// ctor-new-instance checksum=200000
// field-get-static checksum=1400000
// handle-static checksum=200000
// getstatic-other checksum=1400000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class L3W71ClassInitBitBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;

    public static class Target {
        public static int seven = 7;
        public static int one(int x) { return Integer.signum(x); }
    }

    public static class Made {
        public Made() {}
    }

    public static class K0 { public static int s(int x) { return 0 * x; } }
    public static class K1 { public static int s(int x) { return 1; } }
    public static class K2 { public static int s(int x) { return 2; } }
    public static class K3 { public static int s(int x) { return 3; } }
    public static class K4 { public static int s(int x) { return 4; } }
    public static class K5 { public static int s(int x) { return 5; } }
    public static class K6 { public static int s(int x) { return 6; } }
    public static class K7 { public static int s(int x) { return 7; } }

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

    public static void main(String[] args) throws Throwable {
        Method appStatic = Target.class.getDeclaredMethod("one", int.class);
        Method jdkStatic = Integer.class.getDeclaredMethod("signum", int.class);
        Class<?>[] ks = {K0.class, K1.class, K2.class, K3.class, K4.class, K5.class, K6.class, K7.class};
        Method[] many = new Method[ks.length];
        for (int i = 0; i < ks.length; i++) {
            many[i] = ks[i].getDeclaredMethod("s", int.class);
        }
        Constructor<Made> ctor = Made.class.getDeclaredConstructor();
        Field seven = Target.class.getDeclaredField("seven");
        MethodHandle handle = MethodHandles.lookup()
                .findStatic(Target.class, "one", MethodType.methodType(int.class, int.class));

        bench("reflect-static", i -> (Integer) appStatic.invoke(null, i));
        bench("reflect-static-jdk", i -> (Integer) jdkStatic.invoke(null, i));
        // K0 answers 0, K1..K7 answer 1..7: 28 per 8 calls, 700000 in all,
        // plus 1 per call: 900000.
        bench("reflect-static-many", i -> (Integer) many[i & 7].invoke(null, i) + 1);
        bench("ctor-new-instance", i -> ctor.newInstance() != null ? 1 : 0);
        bench("field-get-static", i -> seven.getInt(null));
        bench("handle-static", i -> (int) handle.invokeExact(i));
        bench("getstatic-other", i -> Target.seven);
    }
}
