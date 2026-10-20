// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 81, lane L3 -- the served reflective doors, warm,
// for the wave's switch and the census's new splits:
//
//   CRATONVM_JIT_REFLECT_DOOR_DIRECT=1    a compiled Method.invoke /
//                                         Constructor.newInstance call is
//                                         served by the virtual-MIC helper's
//                                         direct arm (the forced native, as
//                                         the interpreter's cached door runs
//                                         it) instead of the MIC resolution,
//                                         the compile probe and the by-name
//                                         invoke_or_native cascade. JIT only
//                                         (--nojit never reaches the helper).
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1   the doors' census (wave 77) prints,
//                                         to STDERR, one line per kind every
//                                         65536 calls; wave 81 adds the gap
//                                         and call splits: gap_split_n,
//                                         gap_out, gap_java, gap_in,
//                                         gap_in_split_n, gap_in_helper,
//                                         gap_in_funnel, call_split_n,
//                                         call_door, call_exec, call_body
//                                         (run one row per process)
//
// Rows:
//
//   direct-static          a plain static call (the floor; control)
//   reflect-static         Method.invoke(null, i) on a static int method
//   reflect-prebox         Method.invoke(null, args) with a reused Object[]
//   reflect-void-noarg     Method.invoke(null, EMPTY) on a static void no-arg
//                          method (the door's own floor)
//   reflect-virtual        Method.invoke(target, i) on an instance method
//   ctor-noarg             Constructor.newInstance(), public no-arg
//   field-get-int          Field.getInt(null) on a public static int (control:
//                          the Field doors are not on the switch's route)
//   field-get-obj          Field.get(target) on a public instance int (control)
//
// Usage: java L3W81ReflectGateBench [row ...]  (no argument: every row).
// Every row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// A/B (same binary, interleaved, 5 runs, medians of the per-run minima), each
// of --jdk-only and --compatible, JIT (the switch's route) and --nojit (must
// not move):
//   cratonvm <mode> [--nojit] -cp <dir> L3W81ReflectGateBench
//   CRATONVM_JIT_REFLECT_DOOR_DIRECT=1 cratonvm <mode> [--nojit] -cp <dir> L3W81ReflectGateBench
// Census (one row per process; the last stderr lines are the warm ones):
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1 cratonvm --jdk-only -cp <dir> L3W81ReflectGateBench reflect-prebox 2>p.txt
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1 CRATONVM_JIT_REFLECT_DOOR_DIRECT=1 \
//       cratonvm --jdk-only -cp <dir> L3W81ReflectGateBench reflect-prebox 2>pd.txt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-static checksum=20000300000
// reflect-static checksum=20000300000
// reflect-prebox checksum=1400000
// reflect-void-noarg checksum=200000
// reflect-virtual checksum=20000500000
// ctor-noarg checksum=200000
// field-get-int checksum=600000
// field-get-obj checksum=800000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W81ReflectGateBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;
    static final Object[] EMPTY = new Object[0];

    public static class Target {
        public static int VALUE = 3;
        static int noops;

        public int w = 4;

        public static int one(int x) { return x + 1; }
        public static void noop() { noops++; }
        public int two(int x) { return x + 2; }
    }

    public static class Unit {
        public final int v;

        public Unit() {
            this.v = 1;
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
        Method noop = Target.class.getMethod("noop");
        Method two = Target.class.getMethod("two", int.class);
        Constructor<Unit> unit = Unit.class.getConstructor();
        Field value = Target.class.getField("VALUE");
        Field w = Target.class.getField("w");
        Object[] prebox = {Integer.valueOf(6)};
        Target target = new Target();

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i + 2: 20000500000; a constant 7: 1400000; a constant 1: 200000;
        // a constant 3: 600000; a constant 4: 800000.
        bench("direct-static", L3W81ReflectGateBench::direct);
        bench("reflect-static", i -> (Integer) one.invoke(null, i));
        bench("reflect-prebox", i -> (Integer) one.invoke(null, prebox));
        bench("reflect-void-noarg", i -> noop.invoke(null, EMPTY) == null ? 1 : 0);
        bench("reflect-virtual", i -> (Integer) two.invoke(target, i));
        bench("ctor-noarg", i -> unit.newInstance().v);
        bench("field-get-int", i -> value.getInt(null));
        bench("field-get-obj", i -> (Integer) w.get(target));
    }
}
