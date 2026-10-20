// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 69, lane L4: the FIRST call of a field-accessor
// handle -- the call that writes its link -- which
// `CRATONVM_MH_LINKING_CALL_SERVED=1` serves through the link it just wrote
// instead of the door. Every row's handle is called once or twice and then
// dropped, so the linking call is most of each op. Timings go to STDERR: each
// row runs `reps` times (default 7) and one line per row gives the MINIMUM
// ("bench <name> <ns/op>"), after one line per rep ("rep <name> <i> <ns/op>").
// Checksums go to STDOUT once per row ("checksum <name> <value>", from the
// first rep), so stdout is deterministic and can be diffed between runs and
// against HotSpot. Args: [reps].
// Run `--compatible --nojit` and `--jdk-only --nojit`, A/B the switch
// (interleave the binaries' runs; take medians, the host is noisy). Leave
// `CRATONVM_MH_BINDTO_PRELINK_FIELD` unset (with it a bound copy is linked
// at mint and its first call is no longer a linking call).
//
// Rows the switch should move (one fresh handle per op):
//   bind-getter-first-exact   - `(int) getter.bindTo(box).invokeExact()`
//   bind-getter-first-invoke  - `(int) getter.bindTo(box).invoke()`
//   bind-setter-first-exact   - `setter.bindTo(box).invokeExact(3)`
//   bind-setter-first-invoke  - `setter.bindTo(box).invoke(3)`
//   bind-getter-twice         - two `invokeExact`s per fresh bound copy (the
//                               second was already linked: the share of the
//                               first call shows as the difference)
//   find-getter-first         - `lk.findGetter(..)` then one `invokeExact`
//                               (the lookup dominates; a smaller relative move)
//   find-static-getter-first  - `lk.findStaticGetter(..)` then one `invokeExact`
// Controls that must NOT move:
//   bind-getter-object        - `(Object) getter.bindTo(box).invoke()`: an
//                               `asType` site, never linked, the door
//   getter-warm-exact         - one warm handle, `invokeExact` (linked long ago)
//
// `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES=1` (alone, and with
// `CRATONVM_MH_BINDTO_PRELINK_FIELD=1 CRATONVM_MH_BINDTO_FAST_MINT_PRELINK=1`):
//   bind-getter-mixed         - `getter.bindTo(box or subBox).invokeExact()`:
//                               two classes alternately; should fall to about
//                               `bind-getter-alt`
//   bind-setter-mixed         - the setter, two classes alternately
//   bind-getter-alt           - control: two objects of ONE class (hits already)
//   bind-getter-three         - control: three classes in rotation (each bind
//                               misses either way; must not get slower)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum bind-getter-first-exact 600000
// checksum bind-getter-first-invoke 600000
// checksum bind-setter-first-exact 200000
// checksum bind-setter-first-invoke 200000
// checksum bind-getter-twice 1200000
// checksum find-getter-first 60000
// checksum find-static-getter-first 40000
// checksum bind-getter-object 600000
// checksum getter-warm-exact 600000
// checksum bind-getter-mixed 700000
// checksum bind-setter-mixed 200000
// checksum bind-getter-alt 700000
// checksum bind-getter-three 799999
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W69LinkingCallBench {
    public static class Box {
        public static int S = 2;
        public int v = 3;
    }

    public static class SubBox extends Box {
    }

    public static class SubBox2 extends Box {
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
        MethodHandle get = lk.findGetter(Box.class, "v", int.class);
        MethodHandle set = lk.findSetter(Box.class, "v", int.class);
        int n = 200000;
        bench("bind-getter-first-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invokeExact();
            return s;
        });
        bench("bind-getter-first-invoke", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invoke();
            return s;
        });
        bench("bind-setter-first-exact", reps, n, k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invokeExact(3);
            return k;
        });
        bench("bind-setter-first-invoke", reps, n, k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invoke(3);
            return k;
        });
        bench("bind-getter-twice", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandle h = get.bindTo(box);
                s += (int) h.invokeExact();
                s += (int) h.invokeExact();
            }
            return s;
        });
        int m = 20000;
        bench("find-getter-first", reps, m, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                s += (int) lk.findGetter(Box.class, "v", int.class).invokeExact(box);
            }
            return s;
        });
        bench("find-static-getter-first", reps, m, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                s += (int) lk.findStaticGetter(Box.class, "S", int.class).invokeExact();
            }
            return s;
        });
        bench("bind-getter-object", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) get.bindTo(box).invoke();
            return s;
        });
        MethodHandle warm = lk.findGetter(Box.class, "v", int.class);
        bench("getter-warm-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) warm.invokeExact(box);
            return s;
        });

        Box b4 = new Box();
        b4.v = 4;
        SubBox sb = new SubBox();
        sb.v = 4;
        SubBox2 sb2 = new SubBox2();
        sb2.v = 5;
        bench("bind-getter-mixed", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo((i & 1) == 0 ? box : sb).invokeExact();
            return s;
        });
        bench("bind-setter-mixed", reps, n, k -> {
            // Stores each receiver's own value back: box 3, sb 4.
            for (int i = 0; i < k; i++) {
                if ((i & 1) == 0) set.bindTo(box).invokeExact(3);
                else set.bindTo(sb).invokeExact(4);
            }
            return k;
        });
        bench("bind-getter-alt", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo((i & 1) == 0 ? box : b4).invokeExact();
            return s;
        });
        bench("bind-getter-three", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                Box r = i % 3 == 0 ? box : (i % 3 == 1 ? sb : sb2);
                s += (int) get.bindTo(r).invokeExact();
            }
            return s;
        });
    }
}
