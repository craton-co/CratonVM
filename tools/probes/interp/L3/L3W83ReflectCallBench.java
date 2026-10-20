// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 83, lane L3 -- the reflective call's own cost
// (the door census's `call` parts), warm, for the wave's two switches:
//
//   CRATONVM_REFLECT_CTOR_LEAN_ALLOC=1   a reflective construction of an
//                                        initialized class writes its
//                                        defaults from the per-class
//                                        allocation recipe and builds its
//                                        <init> arguments inline (the
//                                        constructor door's call_door)
//   CRATONVM_EXECUTE_FEWER_COPIES=1      interpreter::execute shares the empty
//                                        exception table and probes the
//                                        gate-pass memo with the key it built
//                                        once (call_exec / call_body of every
//                                        reflective call)
//
// The constructor rows also check the defaults a constructor reads before
// it assigns (long, double, float, reference, boolean, a superclass's
// fields): the lean door writes them from the recipe, so a wrong default
// changes the checksum.
//
// Rows:
//
//   reflect-void-noarg   Method.invoke(null, EMPTY) on a static void no-arg
//                        method (no exception table)
//   reflect-static-int   Method.invoke(null, i) on a static int method
//   reflect-catching     Method.invoke(null, i) on a static int method WITH a
//                        handler (a non-empty exception table: the copy stays)
//   ctor-noarg           Constructor.newInstance(), one int field
//   ctor-defaults        Constructor.newInstance() of a subclass whose
//                        constructor reads every field's default first
//   ctor-arg             Constructor.newInstance(Integer) (a pinned argument)
//
// Usage: java L3W83ReflectCallBench [row ...]  (no argument: every row).
// Each row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// A/B (same binary, interleaved, 5 runs, medians of the per-run minima),
// --jdk-only and --compatible, JIT (and --nojit: must not regress):
//   cratonvm <mode> -cp <dir> L3W83ReflectCallBench
//   CRATONVM_REFLECT_CTOR_LEAN_ALLOC=1 cratonvm <mode> -cp <dir> L3W83ReflectCallBench ctor-noarg ctor-defaults ctor-arg
//   CRATONVM_EXECUTE_FEWER_COPIES=1 cratonvm <mode> -cp <dir> L3W83ReflectCallBench
// Census (one row per process; the last two stderr lines are the warm ones):
//   CRATONVM_REFLECT_DOOR_PHASE_STATS=1 cratonvm <mode> -cp <dir> L3W83ReflectCallBench ctor-noarg 2>c.txt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// reflect-void-noarg checksum=200000
// reflect-static-int checksum=20000300000
// reflect-catching checksum=20000500000
// ctor-noarg checksum=200000
// ctor-defaults checksum=1400000
// ctor-arg checksum=20000300000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W83ReflectCallBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;
    static final Object[] EMPTY = new Object[0];

    public static class Target {
        static int noops;

        public static void noop() { noops++; }
        public static int one(int x) { return x + 1; }

        public static int catching(int x) {
            try {
                return Integer.parseInt("2") + x;
            } catch (NumberFormatException e) {
                return -1;
            }
        }
    }

    public static class Unit {
        public final int v;

        public Unit() {
            this.v = 1;
        }
    }

    public static class Base {
        protected long baseLong;
        protected Object baseRef;
    }

    public static class Defaults extends Base {
        public long l;
        public double d;
        public float f;
        public Object o;
        public boolean z;
        public int score;

        public Defaults() {
            // Every field holds its JVM default when the constructor starts.
            int s = 0;
            if (l == 0L) s++;
            if (d == 0.0) s++;
            if (f == 0.0f) s++;
            if (o == null) s++;
            if (!z) s++;
            if (baseLong == 0L) s++;
            if (baseRef == null) s++;
            this.score = s;
            this.l = 5L;
            this.o = this;
        }
    }

    public static class Boxed {
        public final int v;

        public Boxed(Integer v) {
            this.v = v.intValue() + 1;
        }
    }

    interface Body {
        long run(int i) throws Throwable;
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

    public static void main(String[] args) throws Throwable {
        if (args.length > 0) {
            only = new HashSet<>(Arrays.asList(args));
        }
        Method noop = Target.class.getMethod("noop");
        Method one = Target.class.getMethod("one", int.class);
        Method catching = Target.class.getMethod("catching", int.class);
        Constructor<Unit> unit = Unit.class.getConstructor();
        Constructor<Defaults> defaults = Defaults.class.getConstructor();
        Constructor<Boxed> boxed = Boxed.class.getConstructor(Integer.class);

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i + 2: 20000500000; a constant 1: 200000; a constant 7: 1400000.
        bench("reflect-void-noarg", i -> noop.invoke(null, EMPTY) == null ? 1 : 0);
        bench("reflect-static-int", i -> (Integer) one.invoke(null, i));
        bench("reflect-catching", i -> (Integer) catching.invoke(null, i));
        bench("ctor-noarg", i -> unit.newInstance().v);
        bench("ctor-defaults", i -> {
            Defaults x = defaults.newInstance();
            return x.o == x && x.l == 5L ? x.score : -100;
        });
        bench("ctor-arg", i -> boxed.newInstance(Integer.valueOf(i)).v);
    }
}
