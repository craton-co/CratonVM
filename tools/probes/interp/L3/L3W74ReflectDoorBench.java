// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L3 -- the served reflective doors, warm,
// with and without the wave's two switches:
//
//   CRATONVM_REFLECT_INVOKE_PLAN=1     `Method.invoke` reads the member's facts
//                                      (class, name, descriptor, parsed
//                                      parameters and return, modifiers, the
//                                      `@CallerSensitiveAdapter` answer) from
//                                      a per-VM plan, and skips the `exports`
//                                      caller walk for an unnamed-module
//                                      target
//   CRATONVM_FIELD_DOOR_IDENT_MEMO=1   the `Field` doors take their refusal
//                                      identity's text from a per-VM memo
//                                      instead of five `String`s per call
//
// Rows:
//
//   direct-static      a plain static call (the interpreter's floor; control)
//   reflect-static     Method.invoke(null, i) on a static int method (W72 row)
//   reflect-virtual    Method.invoke(target, i) on an instance method
//   reflect-prebox     Method.invoke(null, args) with a reused Object[] (W72 row)
//   reflect-varargs    Method.invoke(null, int[]) on an int... method
//   ctor-new-instance  Constructor.newInstance(i)
//   field-get-static   Field.getInt(null), no setAccessible (W72 row)
//   field-get-inst     Field.getInt(target) on an instance field (W73 row)
//
// Usage: java L3W74ReflectDoorBench [row ...]  (no argument: every row).
// Every row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// A/B (same binary, interleaved, medians of the per-run minima), each of
// --compatible and --jdk-only, --nojit and default:
//   cratonvm <mode> [--nojit] -cp <dir> L3W74ReflectDoorBench
//   CRATONVM_REFLECT_INVOKE_PLAN=1 cratonvm <mode> [--nojit] -cp <dir> L3W74ReflectDoorBench
//   CRATONVM_FIELD_DOOR_IDENT_MEMO=1 cratonvm <mode> [--nojit] -cp <dir> L3W74ReflectDoorBench
// (`--jdk-only` serves the `Field` rows with the natives only under
// CRATONVM_FIELD_DOORS_INTRINSIC=1; add it to both sides of that A/B.)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-static checksum=20000300000
// reflect-static checksum=20000300000
// reflect-virtual checksum=20000500000
// reflect-prebox checksum=1400000
// reflect-varargs checksum=20000300000
// ctor-new-instance checksum=20000100000
// field-get-static checksum=1400000
// field-get-inst checksum=600000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W74ReflectDoorBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;

    public static class Target {
        public static int seven = 7;
        public int count = 3;

        public static int one(int x) { return x + 1; }
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
        Method two = Target.class.getMethod("two", int.class);
        Method sum = Target.class.getMethod("sum", int[].class);
        Constructor<Box> box = Box.class.getConstructor(int.class);
        Object[] prebox = {Integer.valueOf(6)};
        Field seven = Target.class.getDeclaredField("seven");
        Field count = Target.class.getDeclaredField("count");
        Target target = new Target();

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i + 2: 20000500000; i alone: 20000100000; a constant 7: 1400000;
        // a constant 3: 600000.
        bench("direct-static", L3W74ReflectDoorBench::direct);
        bench("reflect-static", i -> (Integer) one.invoke(null, i));
        bench("reflect-virtual", i -> (Integer) two.invoke(target, i));
        bench("reflect-prebox", i -> (Integer) one.invoke(null, prebox));
        bench("reflect-varargs", i -> (Integer) sum.invoke(null, (Object) new int[] {i, 1}));
        bench("ctor-new-instance", i -> box.newInstance(i).v);
        bench("field-get-static", i -> seven.getInt(null));
        bench("field-get-inst", i -> count.getInt(target));
    }
}
