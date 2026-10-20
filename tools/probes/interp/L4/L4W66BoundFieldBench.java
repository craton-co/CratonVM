// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L4: the rows the two wave-66 switches
// move. Timings go to STDERR: each row runs `reps` times (default 7) and one
// line per row gives the MINIMUM ("bench <name> <ns/op>"), after one line per
// rep ("rep <name> <i> <ns/op>"). Checksums go to STDOUT once per row
// ("checksum <name> <value>", from the first rep), so stdout is deterministic
// and can be diffed between runs and against HotSpot. Args: [reps].
// Run `--compatible` and `--jdk-only`, interleaving switch settings.
//
//   bound-getter-exact     - `findGetter(Box, v).bindTo(box)` through
//                            `invokeExact()`: the door before
//                            `CRATONVM_MH_FIELD_LINK_BOUND=1`, the link with it
//   bound-setter-exact     - `findSetter(Box, v).bindTo(box)` through
//                            `invokeExact(3)`
//   bound-ref-setter-exact - a `CharSequence` field's bound setter at
//                            `(CharSequence)void` (the link's value check)
//   bind-getter            - `getter.bindTo(box)` alone: the slow mint before
//                            `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS=1` (with
//                            `CRATONVM_MH_BINDTO_FAST_MINT`, default on), the
//                            memoised mint with it
//   bind-getter-exact      - `getter.bindTo(box).invokeExact()` per call: both
//                            switches (a fresh bound copy links on its first
//                            call, so the link pays on the second only)
//   bind-setter            - `setter.bindTo(box).invokeExact(3)` per call
//   unbound-getter-exact   - control: the unbound getter's link either way
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum bound-getter-exact 600000
// checksum bound-setter-exact 200000
// checksum bound-ref-setter-exact 200000
// checksum bind-getter 200000
// checksum bind-getter-exact 600000
// checksum bind-setter 200000
// checksum unbound-getter-exact 600000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W66BoundFieldBench {
    public static class Box {
        public int v = 3;
        public CharSequence cs = "init";
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static void bench(String name, int reps, int n, Body b) throws Throwable {
        b.run(Math.max(1, n / 10));
        double best = Double.MAX_VALUE;
        long checksum = 0;
        for (int rep = 0; rep < reps; rep++) {
            long t0 = System.nanoTime();
            long s = b.run(n);
            long t1 = System.nanoTime();
            double ns = (t1 - t0) / (double) n;
            System.err.printf(java.util.Locale.ROOT, "rep %s %d %.1f%n", name, rep, ns);
            best = Math.min(best, ns);
            if (rep == 0) {
                checksum = s;
            }
        }
        System.err.printf(java.util.Locale.ROOT, "bench %s %.1f%n", name, best);
        System.out.println("checksum " + name + " " + checksum);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 7;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Box box = new Box();
        MethodHandle get = lk.findGetter(Box.class, "v", int.class);
        MethodHandle set = lk.findSetter(Box.class, "v", int.class);
        MethodHandle setCs = lk.findSetter(Box.class, "cs", CharSequence.class);
        MethodHandle bget = get.bindTo(box);
        MethodHandle bset = set.bindTo(box);
        MethodHandle bsetCs = setCs.bindTo(box);
        CharSequence x = "x";
        int n = 200000;
        bench("bound-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bget.invokeExact();
            return s;
        });
        bench("bound-setter-exact", reps, n, k -> {
            for (int i = 0; i < k; i++) bset.invokeExact(3);
            return k;
        });
        bench("bound-ref-setter-exact", reps, n, k -> {
            for (int i = 0; i < k; i++) bsetCs.invokeExact(x);
            return k;
        });
        bench("bind-getter", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                if (get.bindTo(box) != null) s++;
            }
            return s;
        });
        bench("bind-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invokeExact();
            return s;
        });
        bench("bind-setter", reps, n, k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invokeExact(3);
            return k;
        });
        bench("unbound-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.invokeExact(box);
            return s;
        });
    }
}
