// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L3 -- the `Field` value doors under
// `--jdk-only`, with and without CRATONVM_FIELD_DOORS_INTRINSIC (the doors
// served by the natives `--compatible` runs, as reviewed `Intrinsic`s, instead
// of the JDK's `Field` bytecode: `Reflection.getCallerClass`, `checkAccess`,
// `refersTo`, the accessor and its `getter.invokeExact` door). Rows (the
// W72 field rows plus the instance and write sides):
//
//   direct-static      a plain static call (the interpreter's floor; control)
//   field-get-static   Field.getInt(null), no setAccessible (W72 row)
//   field-get-acc      the same after setAccessible(true) (W72 row)
//   field-get-obj      Field.get(null), boxing (W72 row)
//   field-get-inst     Field.getInt(target) on an instance field
//   field-set-static   Field.setInt(null, i)
//   field-set-inst     Field.setInt(target, i) on an instance field
//
// Usage: java L3W73FieldDoorBench [row ...]  (no argument: every row).
// Every row runs REPS timed repetitions after one untimed pass and prints the
// MINIMUM. Stdout: a deterministic checksum line per row (no "ns/") and a
// timing line per row ("<row> <x> ns/op").
//
// A/B (same binary, interleaved, medians of the per-run minima):
//   cratonvm --jdk-only   --nojit -cp <dir> L3W73FieldDoorBench
//   CRATONVM_FIELD_DOORS_INTRINSIC=1 cratonvm --jdk-only --nojit -cp <dir> L3W73FieldDoorBench
//   cratonvm --compatible --nojit -cp <dir> L3W73FieldDoorBench          (the target)
// and the same three without --nojit.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct-static checksum=20000300000
// field-get-static checksum=1400000
// field-get-acc checksum=1400000
// field-get-obj checksum=1400000
// field-get-inst checksum=600000
// field-set-static checksum=20000100000
// field-set-inst checksum=20000100000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

public class L3W73FieldDoorBench {
    static final int ITERS = 200_000;
    static final int REPS = 5;

    public static class Target {
        public static int seven = 7;
        public static int sink;
        public int three = 3;
        public int slot;

        public static int one(int x) {
            return x + 1;
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

    static int direct(int i) {
        return Target.one(i);
    }

    public static void main(String[] args) throws Throwable {
        if (args.length > 0) {
            only = new HashSet<>(Arrays.asList(args));
        }
        Target target = new Target();
        Field seven = Target.class.getDeclaredField("seven");
        Field sevenAcc = Target.class.getDeclaredField("seven");
        sevenAcc.setAccessible(true);
        Field three = Target.class.getDeclaredField("three");
        Field sink = Target.class.getDeclaredField("sink");
        Field slot = Target.class.getDeclaredField("slot");

        // i + 1 for i in [1, ITERS] summed: ITERS(ITERS+1)/2 + ITERS = 20000300000;
        // i alone: 20000100000; a constant 7: 1400000; a constant 3: 600000.
        bench("direct-static", L3W73FieldDoorBench::direct);
        bench("field-get-static", i -> seven.getInt(null));
        bench("field-get-acc", i -> sevenAcc.getInt(null));
        bench("field-get-obj", i -> (Integer) seven.get(null));
        bench("field-get-inst", i -> three.getInt(target));
        bench("field-set-static", i -> {
            sink.setInt(null, i);
            return Target.sink;
        });
        bench("field-set-inst", i -> {
            slot.setInt(target, i);
            return target.slot;
        });
    }
}
