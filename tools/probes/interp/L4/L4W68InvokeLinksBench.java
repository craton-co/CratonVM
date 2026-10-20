// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 68, lane L4: the rows the two wave-68 switches
// move. Timings go to STDERR: each row runs `reps` times (default 7) and one
// line per row gives the MINIMUM ("bench <name> <ns/op>"), after one line per
// rep ("rep <name> <i> <ns/op>"). Checksums go to STDOUT once per row
// ("checksum <name> <value>", from the first rep), so stdout is deterministic
// and can be diffed between runs and against HotSpot. Args: [reps].
// Run `--compatible` and `--jdk-only`, interleaving switch settings; take
// medians over several runs (the host's timings are noisy).
//
// `CRATONVM_MH_INVOKE_LINKS_FIELD=1` (with `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`,
// default on) -- handles only ever called through `invoke` (no warm-up
// `invokeExact`, unlike `L4W67PrelinkBench`):
//   getter-invoke-only        - `(int) getter.invoke(box)`
//   setter-invoke-only        - `setter.invoke(box, 3)`
//   static-getter-invoke-only - `(int) staticGetter.invoke()`
//   bound-getter-invoke-only  - `(int) getter.bindTo(box)` (once) `.invoke()`
//   bound-setter-invoke-only  - `setter.bindTo(box)` (once) `.invoke(3)`
//   getter-invoke-object      - control: `(int) getter.invoke((Object) box)`,
//                               an `asType` site, the door either way
//
// `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK=1` (with
// `CRATONVM_MH_BINDTO_PRELINK_FIELD=1`) -- a fast-mint hit replays the link:
//   bind-getter-exact         - `getter.bindTo(box).invokeExact()` per call
//   bind-setter               - `setter.bindTo(box).invokeExact(3)` per call
//   bind-getter-alt           - `getter.bindTo(a or b).invokeExact()`, two
//                               objects of one class alternately
//   bind-getter-mixed         - control: `Box` / `SubBox` receivers
//                               alternately (no line hits; slow path)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum getter-invoke-only 600000
// checksum setter-invoke-only 200000
// checksum static-getter-invoke-only 400000
// checksum bound-getter-invoke-only 600000
// checksum bound-setter-invoke-only 200000
// checksum getter-invoke-object 600000
// checksum bind-getter-exact 600000
// checksum bind-setter 200000
// checksum bind-getter-alt 700000
// checksum bind-getter-mixed 700000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W68InvokeLinksBench {
    public static class Box {
        public static int S = 2;
        public int v = 3;
    }

    public static class SubBox extends Box {
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static void bench(String name, int reps, int n, Body b) throws Throwable {
        b.run(Math.max(2, n / 10));
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
        // `invoke` only: none of these handles ever sees `invokeExact`.
        MethodHandle get = lk.findGetter(Box.class, "v", int.class);
        MethodHandle set = lk.findSetter(Box.class, "v", int.class);
        MethodHandle sget = lk.findStaticGetter(Box.class, "S", int.class);
        MethodHandle bget = lk.findGetter(Box.class, "v", int.class).bindTo(box);
        MethodHandle bset = lk.findSetter(Box.class, "v", int.class).bindTo(box);
        MethodHandle getObj = lk.findGetter(Box.class, "v", int.class);
        int n = 200000;
        bench("getter-invoke-only", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.invoke(box);
            return s;
        });
        bench("setter-invoke-only", reps, n, k -> {
            for (int i = 0; i < k; i++) set.invoke(box, 3);
            return k;
        });
        bench("static-getter-invoke-only", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) sget.invoke();
            return s;
        });
        bench("bound-getter-invoke-only", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bget.invoke();
            return s;
        });
        bench("bound-setter-invoke-only", reps, n, k -> {
            for (int i = 0; i < k; i++) bset.invoke(3);
            return k;
        });
        bench("getter-invoke-object", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) getObj.invoke((Object) box);
            return s;
        });

        // `bindTo` per call: the fast-mint line and its pre-link.
        MethodHandle bindGet = lk.findGetter(Box.class, "v", int.class);
        MethodHandle bindSet = lk.findSetter(Box.class, "v", int.class);
        Box a = new Box();
        Box b = new Box();
        b.v = 4;
        SubBox sb = new SubBox();
        sb.v = 4;
        bench("bind-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bindGet.bindTo(box).invokeExact();
            return s;
        });
        bench("bind-setter", reps, n, k -> {
            for (int i = 0; i < k; i++) bindSet.bindTo(box).invokeExact(3);
            return k;
        });
        bench("bind-getter-alt", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bindGet.bindTo((i & 1) == 0 ? a : b).invokeExact();
            return s;
        });
        bench("bind-getter-mixed", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bindGet.bindTo((i & 1) == 0 ? a : sb).invokeExact();
            return s;
        });
    }
}
