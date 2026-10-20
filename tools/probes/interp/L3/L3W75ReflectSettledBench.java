// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L3 -- the served `Method.invoke` and
// `Constructor.newInstance` doors, warm, with and without the wave's two
// switches (both meaningful with the default-on CRATONVM_REFLECT_INVOKE_PLAN):
//
//   CRATONVM_REFLECT_INVOKE_SETTLED=1    a planned static target outside
//                                        java/ and jdk/ dispatches through the
//                                        settling static door with the owner
//                                        its plan remembers (no by-name
//                                        `invoke_by_class_id` tail)
//   CRATONVM_REFLECT_CTOR_PLAN_PER_VM=1  constructor plans live in the per-VM
//                                        reflect-door caches (read lock, the
//                                        class flags kept), and a public or
//                                        overridden constructor skips the
//                                        `exports` caller walk when it cannot
//                                        refuse
//
// Rows:
//
//   direct-static       a plain static call (the floor; control)
//   reflect-static      Method.invoke(null, i) on a static int method (W74 row)
//   reflect-static-obj  Method.invoke(null, "abcde") on a static (String)I method
//   reflect-prebox      Method.invoke(null, args) with a reused Object[] (W74 row)
//   reflect-varargs     Method.invoke(null, int[]) on an int... method (W74 row)
//   reflect-virtual     Method.invoke(target, i) on an instance method (control:
//                       no switch of this wave touches it)
//   ctor-new-instance   Constructor.newInstance(i), public (W74 row)
//   ctor-noarg          Constructor.newInstance(), public no-arg
//   ctor-accessible     Constructor.newInstance(i) after setAccessible(true), private
//   ctor-jdk-public     StringBuilder(String) through Constructor.newInstance
//
// Usage: java L3W75ReflectSettledBench [row ...]  (no argument: every row).
// Every row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// A/B (same binary, interleaved, medians of the per-run minima), each of
// --compatible and --jdk-only, --nojit and default:
//   cratonvm <mode> [--nojit] -cp <dir> L3W75ReflectSettledBench
//   CRATONVM_REFLECT_INVOKE_SETTLED=1 cratonvm <mode> [--nojit] -cp <dir> L3W75ReflectSettledBench
//   CRATONVM_REFLECT_CTOR_PLAN_PER_VM=1 cratonvm <mode> [--nojit] -cp <dir> L3W75ReflectSettledBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-static checksum=20000300000
// reflect-static checksum=20000300000
// reflect-static-obj checksum=1000000
// reflect-prebox checksum=1400000
// reflect-varargs checksum=20000300000
// reflect-virtual checksum=20000500000
// ctor-new-instance checksum=20000100000
// ctor-noarg checksum=200000
// ctor-accessible checksum=20000100000
// ctor-jdk-public checksum=400000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W75ReflectSettledBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;

    public static class Target {
        public static int one(int x) { return x + 1; }
        public static int len(String s) { return s.length(); }
        public int two(int x) { return x + 2; }
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
        Method two = Target.class.getMethod("two", int.class);
        Method sum = Target.class.getMethod("sum", int[].class);
        Constructor<Box> box = Box.class.getConstructor(int.class);
        Constructor<Unit> unit = Unit.class.getConstructor();
        Constructor<Priv> priv = Priv.class.getDeclaredConstructor(int.class);
        priv.setAccessible(true);
        Constructor<StringBuilder> sb = StringBuilder.class.getConstructor(String.class);
        Object[] prebox = {Integer.valueOf(6)};
        Target target = new Target();

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i + 2: 20000500000; i alone: 20000100000; a constant 5: 1000000;
        // a constant 7: 1400000; a constant 1: 200000; a constant 2: 400000.
        bench("direct-static", L3W75ReflectSettledBench::direct);
        bench("reflect-static", i -> (Integer) one.invoke(null, i));
        bench("reflect-static-obj", i -> (Integer) len.invoke(null, "abcde"));
        bench("reflect-prebox", i -> (Integer) one.invoke(null, prebox));
        bench("reflect-varargs", i -> (Integer) sum.invoke(null, (Object) new int[] {i, 1}));
        bench("reflect-virtual", i -> (Integer) two.invoke(target, i));
        bench("ctor-new-instance", i -> box.newInstance(i).v);
        bench("ctor-noarg", i -> unit.newInstance().v);
        bench("ctor-accessible", i -> priv.newInstance(i).v);
        bench("ctor-jdk-public", i -> sb.newInstance("ab").length());
    }
}
