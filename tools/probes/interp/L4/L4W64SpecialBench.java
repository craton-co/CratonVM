// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L4: the door rows
// `CRATONVM_MH_SPECIAL_BY_OWNER_ID` (a `findSpecial` handle dispatches on its
// class by id, not by re-resolving `MH_CLASS`'s name per call) moves.
// Timings go to STDERR, one line per row ("bench <name> <ns/op>"); the
// checksums go to STDOUT ("checksum <name> <value>"). Args: [reps].
// Interleave A/B (switch off vs `=1`, `--compatible`), medians of >= 5 reps.
//
//   special-invoke   - `findSpecial(Mid, who, Sub)` through `invoke`
//   special-exact    - its `asType((Object)int)` copy through `invokeExact`
//   special-bound    - its `bindTo(sub)` copy through `invoke()`
//   virtual-invoke   - `findVirtual(Mid, who)` through `invoke` (control)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum special-invoke 400000
// checksum special-exact 400000
// checksum special-bound 400000
// checksum virtual-invoke 600000
// HOTSPOT_EXPECTED_END
// (one block per rep; the block above is `reps = 1`.)
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W64SpecialBench {
    public static class Mid {
        public int who() {
            return 2;
        }
    }

    public static class Sub extends Mid {
        @Override
        public int who() {
            return 3;
        }

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static void bench(String name, int n, Body b) throws Throwable {
        b.run(Math.max(1, n / 10));
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        System.err.printf(java.util.Locale.ROOT, "bench %s %.1f%n", name, (t1 - t0) / (double) n);
        System.out.println("checksum " + name + " " + s);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        MethodType mt = MethodType.methodType(int.class);
        MethodHandle special = Sub.lookup().findSpecial(Mid.class, "who", mt, Sub.class);
        MethodHandle exact = special.asType(MethodType.methodType(int.class, Object.class));
        Object sub = new Sub();
        MethodHandle bound = special.bindTo(sub);
        MethodHandle virt = MethodHandles.lookup().findVirtual(Mid.class, "who", mt);
        int n = 200000;
        for (int rep = 0; rep < reps; rep++) {
            bench("special-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) special.invoke(sub);
                return s;
            });
            bench("special-exact", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) exact.invokeExact(sub);
                return s;
            });
            bench("special-bound", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) bound.invoke();
                return s;
            });
            bench("virtual-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) virt.invoke(sub);
                return s;
            });
        }
    }
}
