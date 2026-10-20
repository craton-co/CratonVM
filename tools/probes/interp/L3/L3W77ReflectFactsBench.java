// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L3 -- the served `Method.invoke` and
// `Constructor.newInstance` doors, warm, for the wave's two switches
// (meaningful with the default-on CRATONVM_REFLECT_INVOKE_PLAN and
// CRATONVM_REFLECT_DOOR_WORD_BATCH):
//
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1   the doors time their phases and
//                                         print, to STDERR, one line per kind
//                                         every 65536 calls:
//                                           reflect-door-phases kind=invoke calls=65536
//                                             total=.. enter=.. words=.. gates=.. init=..
//                                             recv=.. args=.. select=.. call=.. box=..
//                                             ret=.. leave=.. gap=.. gap_n=.. mark=.. (ns/call)
//                                         (a measurement instrument: run one
//                                         row per process so the lines belong
//                                         to that row; the last lines of a row
//                                         are the warm ones)
//   CRATONVM_REFLECT_INVOKE_PLAN_FACTS=1  a `Method` plan keeps its declaring
//                                         class's flags (no class-manager read
//                                         for the class half of the access
//                                         check), an exact-class receiver of a
//                                         virtual call skips the two selection
//                                         reads, `crosses_package` compares the
//                                         interned names, and the boxing
//                                         wrapper skips the return-shape check
//                                         of a primitive the body boxed itself
//
// Rows:
//
//   direct-static          a plain static call (the floor; control)
//   reflect-static         Method.invoke(null, i) on a static int method
//   reflect-static-obj     Method.invoke(null, "abcde") on a static (String)I method
//   reflect-prebox         Method.invoke(null, args) with a reused Object[]
//   reflect-void-noarg     Method.invoke(null, EMPTY) on a static void no-arg
//                          method (no argument, no box: the door's own floor)
//   reflect-virtual        Method.invoke(target, i) on an instance method,
//                          receiver of the declaring class (exact)
//   reflect-virtual-sub    the same Method, receiver of a subclass that does
//                          not override it (not exact: must not move)
//   reflect-virtual-pkg    a package-private instance method, exact receiver
//   reflect-object-return  Method.invoke(null, "ab") on a static (String)String
//   ctor-noarg             Constructor.newInstance(), public no-arg
//
// Usage: java L3W77ReflectFactsBench [row ...]  (no argument: every row).
// Every row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// A/B (same binary, interleaved, medians of the per-run minima), each of
// --compatible and --jdk-only, --nojit and default:
//   cratonvm <mode> [--nojit] -cp <dir> L3W77ReflectFactsBench
//   CRATONVM_REFLECT_INVOKE_PLAN_FACTS=1 cratonvm <mode> [--nojit] -cp <dir> L3W77ReflectFactsBench
// Census (one row per process; stderr carries the phase lines):
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1 cratonvm <mode> -cp <dir> L3W77ReflectFactsBench reflect-prebox
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1 cratonvm <mode> -cp <dir> L3W77ReflectFactsBench reflect-virtual
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1 cratonvm <mode> -cp <dir> L3W77ReflectFactsBench reflect-void-noarg
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1 cratonvm <mode> -cp <dir> L3W77ReflectFactsBench ctor-noarg
// The switch OFF must not move against the wave-76 binary (cross-binary run
// of L3W76ReflectBatchBench).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-static checksum=20000300000
// reflect-static checksum=20000300000
// reflect-static-obj checksum=1000000
// reflect-prebox checksum=1400000
// reflect-void-noarg checksum=200000
// reflect-virtual checksum=20000500000
// reflect-virtual-sub checksum=20000500000
// reflect-virtual-pkg checksum=20000900000
// reflect-object-return checksum=400000
// ctor-noarg checksum=200000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W77ReflectFactsBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;
    static final Object[] EMPTY = new Object[0];

    public static class Target {
        static int noops;

        public static int one(int x) { return x + 1; }
        public static int len(String s) { return s.length(); }
        public static String id(String s) { return s; }
        public static void noop() { noops++; }
        public int two(int x) { return x + 2; }
        int four(int x) { return x + 4; }
    }

    public static class SubTarget extends Target {
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
        Method len = Target.class.getMethod("len", String.class);
        Method id = Target.class.getMethod("id", String.class);
        Method noop = Target.class.getMethod("noop");
        Method two = Target.class.getMethod("two", int.class);
        Method four = Target.class.getDeclaredMethod("four", int.class);
        Constructor<Unit> unit = Unit.class.getConstructor();
        Object[] prebox = {Integer.valueOf(6)};
        Target target = new Target();
        Target sub = new SubTarget();

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i + 2: 20000500000; i + 4: 20000900000; a constant 5: 1000000;
        // a constant 7: 1400000; a constant 2: 400000; a constant 1: 200000.
        bench("direct-static", L3W77ReflectFactsBench::direct);
        bench("reflect-static", i -> (Integer) one.invoke(null, i));
        bench("reflect-static-obj", i -> (Integer) len.invoke(null, "abcde"));
        bench("reflect-prebox", i -> (Integer) one.invoke(null, prebox));
        bench("reflect-void-noarg", i -> noop.invoke(null, EMPTY) == null ? 1 : 0);
        bench("reflect-virtual", i -> (Integer) two.invoke(target, i));
        bench("reflect-virtual-sub", i -> (Integer) two.invoke(sub, i));
        bench("reflect-virtual-pkg", i -> (Integer) four.invoke(target, i));
        bench("reflect-object-return", i -> ((String) id.invoke(null, "ab")).length());
        bench("ctor-noarg", i -> unit.newInstance().v);
    }
}
