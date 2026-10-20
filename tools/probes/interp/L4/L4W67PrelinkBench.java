// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 67, lane L4: the rows the two wave-67 switches
// move. Timings go to STDERR: each row runs `reps` times (default 7) and one
// line per row gives the MINIMUM ("bench <name> <ns/op>"), after one line per
// rep ("rep <name> <i> <ns/op>"). Checksums go to STDOUT once per row
// ("checksum <name> <value>", from the first rep), so stdout is deterministic
// and can be diffed between runs and against HotSpot. Args: [reps].
// Run `--compatible` and `--jdk-only`, interleaving switch settings.
//
// `CRATONVM_MH_BINDTO_PRELINK_FIELD=1` (with `CRATONVM_MH_FIELD_LINK_BOUND`,
// default on) -- a fresh bound copy's first call is linked:
//   bind-getter-exact    - `getter.bindTo(box).invokeExact()` per call
//   bind-setter          - `setter.bindTo(box).invokeExact(3)` per call
//   bind-ref-setter      - control: a `CharSequence` setter's fresh copy (a
//                          value check: left to its first call either way)
//
// `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE=1` -- `invoke` at the declared type:
//   getter-invoke        - `(int) getter.invoke(box)`
//   setter-invoke        - `setter.invoke(box, 3)`
//   bound-getter-invoke  - `(int) getter.bindTo(box)` (once) `.invoke()`
//   bound-setter-invoke  - `setter.bindTo(box)` (once) `.invoke(3)`
//   static-getter-invoke - `(int) staticGetter.invoke()`
//   getter-invoke-object - control: `(int) getter.invoke((Object) box)`, an
//                          `asType` site, the door either way
//   bound-getter-exact   - control: the bound link through `invokeExact`
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum bind-getter-exact 600000
// checksum bind-setter 200000
// checksum bind-ref-setter 200000
// checksum getter-invoke 600000
// checksum setter-invoke 200000
// checksum bound-getter-invoke 600000
// checksum bound-setter-invoke 200000
// checksum static-getter-invoke 400000
// checksum getter-invoke-object 600000
// checksum bound-getter-exact 600000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W67PrelinkBench {
    public static class Box {
        public static int S = 2;
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
        MethodHandle sget = lk.findStaticGetter(Box.class, "S", int.class);
        MethodHandle bget = get.bindTo(box);
        MethodHandle bset = set.bindTo(box);
        CharSequence x = "x";
        int n = 200000;
        // Link every handle the `invoke` rows use (`invoke` itself links nothing).
        for (int i = 0; i < 10; i++) {
            if ((int) get.invokeExact(box) + (int) bget.invokeExact() + (int) sget.invokeExact() < 0) {
                System.out.println("unreachable");
            }
            set.invokeExact(box, 3);
            bset.invokeExact(3);
        }
        bench("bind-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invokeExact();
            return s;
        });
        bench("bind-setter", reps, n, k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invokeExact(3);
            return k;
        });
        bench("bind-ref-setter", reps, n, k -> {
            for (int i = 0; i < k; i++) setCs.bindTo(box).invokeExact(x);
            return k;
        });
        bench("getter-invoke", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.invoke(box);
            return s;
        });
        bench("setter-invoke", reps, n, k -> {
            for (int i = 0; i < k; i++) set.invoke(box, 3);
            return k;
        });
        bench("bound-getter-invoke", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bget.invoke();
            return s;
        });
        bench("bound-setter-invoke", reps, n, k -> {
            for (int i = 0; i < k; i++) bset.invoke(3);
            return k;
        });
        bench("static-getter-invoke", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) sget.invoke();
            return s;
        });
        bench("getter-invoke-object", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.invoke((Object) box);
            return s;
        });
        bench("bound-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bget.invokeExact();
            return s;
        });
    }
}
