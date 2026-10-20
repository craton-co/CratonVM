// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L3 -- the served `Method.invoke` and
// `Constructor.newInstance` doors, warm, with and without the wave's switch
// (meaningful with the default-on CRATONVM_REFLECT_INVOKE_PLAN):
//
//   CRATONVM_REFLECT_DOOR_WORD_BATCH=1  a planned call reads its mirror words
//                                       in one batch, builds one argument
//                                       vector (the mirror's class id read
//                                       once), and the boxing wrapper takes
//                                       the return descriptor from the plan
//                                       instead of reading a descriptor
//                                       `String` off the mirror and parsing
//                                       it per call
//
// Rows:
//
//   direct-static          a plain static call (the floor; control)
//   reflect-static         Method.invoke(null, i) on a static int method
//   reflect-static-obj     Method.invoke(null, "abcde") on a static (String)I method
//   reflect-prebox         Method.invoke(null, args) with a reused Object[]
//   reflect-varargs        Method.invoke(null, int[]) on an int... method
//   reflect-virtual        Method.invoke(target, i) on an instance method
//   reflect-accessible     Method.invoke(null, i) on a private static method
//                          after setAccessible(true) (the `override` word set)
//   reflect-object-return  Method.invoke(null, "ab") on a static (String)String
//   ctor-new-instance      Constructor.newInstance(i), public
//   ctor-noarg             Constructor.newInstance(), public no-arg
//   ctor-accessible        Constructor.newInstance(i) after setAccessible(true), private
//
// Usage: java L3W76ReflectBatchBench [row ...]  (no argument: every row).
// Every row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// A/B (same binary, interleaved, medians of the per-run minima), each of
// --compatible and --jdk-only, --nojit and default:
//   cratonvm <mode> [--nojit] -cp <dir> L3W76ReflectBatchBench
//   CRATONVM_REFLECT_DOOR_WORD_BATCH=1 cratonvm <mode> [--nojit] -cp <dir> L3W76ReflectBatchBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-static checksum=20000300000
// reflect-static checksum=20000300000
// reflect-static-obj checksum=1000000
// reflect-prebox checksum=1400000
// reflect-varargs checksum=20000300000
// reflect-virtual checksum=20000500000
// reflect-accessible checksum=20000700000
// reflect-object-return checksum=400000
// ctor-new-instance checksum=20000100000
// ctor-noarg checksum=200000
// ctor-accessible checksum=20000100000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W76ReflectBatchBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;

    public static class Target {
        public static int one(int x) { return x + 1; }
        public static int len(String s) { return s.length(); }
        public static String id(String s) { return s; }
        public int two(int x) { return x + 2; }
        private static int three(int x) { return x + 3; }
        public static int sum(int... xs) {
            int s = 0;
            for (int x : xs) {
                s += x;
            }
            return s;
        }
    }

    public static class Box {
        public final int v;

        public Box(int v) {
            this.v = v;
        }
    }

    public static class Unit {
        public final int v;

        public Unit() {
            this.v = 1;
        }
    }

    public static class Priv {
        final int v;

        private Priv(int v) {
            this.v = v;
        }
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

    public static void main(String[] args) throws Throwable {
        if (args.length > 0) {
            only = new HashSet<>(Arrays.asList(args));
        }
        Method one = Target.class.getMethod("one", int.class);
        Method len = Target.class.getMethod("len", String.class);
        Method id = Target.class.getMethod("id", String.class);
        Method two = Target.class.getMethod("two", int.class);
        Method three = Target.class.getDeclaredMethod("three", int.class);
        three.setAccessible(true);
        Method sum = Target.class.getMethod("sum", int[].class);
        Constructor<Box> box = Box.class.getConstructor(int.class);
        Constructor<Unit> unit = Unit.class.getConstructor();
        Constructor<Priv> priv = Priv.class.getDeclaredConstructor(int.class);
        priv.setAccessible(true);
        Object[] prebox = {Integer.valueOf(6)};
        Target target = new Target();

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i + 2: 20000500000; i + 3: 20000700000; i alone: 20000100000;
        // a constant 5: 1000000; a constant 7: 1400000; a constant 2: 400000;
        // a constant 1: 200000.
        bench("direct-static", L3W76ReflectBatchBench::direct);
        bench("reflect-static", i -> (Integer) one.invoke(null, i));
        bench("reflect-static-obj", i -> (Integer) len.invoke(null, "abcde"));
        bench("reflect-prebox", i -> (Integer) one.invoke(null, prebox));
        bench("reflect-varargs", i -> (Integer) sum.invoke(null, (Object) new int[] {i, 1}));
        bench("reflect-virtual", i -> (Integer) two.invoke(target, i));
        bench("reflect-accessible", i -> (Integer) three.invoke(null, i));
        bench("reflect-object-return", i -> ((String) id.invoke(null, "ab")).length());
        bench("ctor-new-instance", i -> box.newInstance(i).v);
        bench("ctor-noarg", i -> unit.newInstance().v);
        bench("ctor-accessible", i -> priv.newInstance(i).v);
    }
}
