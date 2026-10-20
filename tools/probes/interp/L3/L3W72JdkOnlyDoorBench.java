// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L3 -- where `--jdk-only --nojit` spends a
// reflective / method-handle door call that `--compatible` does not
// (proposal i72-L3-proposal-jdk-only-doors-run-the-jdks-java-around-the-class-init-test).
// L3W71ClassInitBitBench measured `--jdk-only --nojit` reflect-static ~6200,
// field-get-static ~5500, handle-static ~1900 ns/op against `--compatible`
// 3400 / 1400 / 250, and the bitmap-first class-init test moved none of the
// `--jdk-only` rows. These rows split each door into the parts the two modes
// run differently, so the A/B is a subtraction:
//
//   direct-static      a plain static call (the interpreter's own floor)
//   box-unbox          Integer.valueOf(i).intValue() (JDK library Java the
//                      reflective rows run around the door, per call)
//   reflect-static     Method.invoke(null, i) (W71 row; boxes the argument
//                      and unboxes the result)
//   reflect-prebox     Method.invoke(null, args) with a reused Object[]: the
//                      door alone, minus the caller's boxing
//   field-get-static   Field.getInt(null) (W71 row; no setAccessible)
//   field-get-acc      the same Field after setAccessible(true): drops the
//                      JDK's checkAccess / caller-class step where the JDK's
//                      Field bytecode runs
//   field-get-obj      Field.get(null) (the Object getter, another accessor)
//   handle-static      findStatic(..).invokeExact(i) (W71 row)
//   handle-invoke      the same handle through invoke(i) (asType-free here:
//                      the site type is the handle's)
//
// Usage: java L3W72JdkOnlyDoorBench [row ...]  (no argument: every row). One
// row at a time is what the sampling profiler wants:
//   CRATONVM_PROFILE_SAMPLE_MS=1 cratonvm --jdk-only --nojit -cp <dir> L3W72JdkOnlyDoorBench field-get-static
//
// Every row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-static checksum=20000300000
// box-unbox checksum=20000100000
// reflect-static checksum=20000300000
// reflect-prebox checksum=1400000
// field-get-static checksum=1400000
// field-get-acc checksum=1400000
// field-get-obj checksum=1400000
// handle-static checksum=20000300000
// handle-invoke checksum=20000300000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W72JdkOnlyDoorBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;

    public static class Target {
        public static int seven = 7;
        public static int one(int x) { return x + 1; }
    }

    interface Body {
        int run(int i) throws Throwable;
    }

    static Set<String> only;

    static void bench(String name, Body body) throws Throwable {
        if (only != null && !only.contains(name)) {
            return;
        }
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

    static int direct(int i) { return Target.one(i); }
    static int boxUnbox(int i) { return Integer.valueOf(i).intValue(); }

    public static void main(String[] args) throws Throwable {
        if (args.length > 0) {
            only = new HashSet<>(Arrays.asList(args));
        }
        Method one = Target.class.getDeclaredMethod("one", int.class);
        Object[] prebox = {Integer.valueOf(6)};
        Field seven = Target.class.getDeclaredField("seven");
        Field sevenAcc = Target.class.getDeclaredField("seven");
        sevenAcc.setAccessible(true);
        MethodHandle handle = MethodHandles.lookup()
                .findStatic(Target.class, "one", MethodType.methodType(int.class, int.class));

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i alone: 20000100000; a constant 7: 1400000.
        bench("direct-static", L3W72JdkOnlyDoorBench::direct);
        bench("box-unbox", L3W72JdkOnlyDoorBench::boxUnbox);
        bench("reflect-static", i -> (Integer) one.invoke(null, i));
        bench("reflect-prebox", i -> (Integer) one.invoke(null, prebox));
        bench("field-get-static", i -> seven.getInt(null));
        bench("field-get-acc", i -> sevenAcc.getInt(null));
        bench("field-get-obj", i -> (Integer) seven.get(null));
        bench("handle-static", i -> (int) handle.invokeExact(i));
        bench("handle-invoke", i -> (int) handle.invoke(i));
    }
}
