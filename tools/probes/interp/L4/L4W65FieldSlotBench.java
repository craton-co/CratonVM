// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 65, lane L4: the rows the two wave-65 switches
// move. Timings go to STDERR: each row runs `reps` times (default 7) and one
// line per row gives the MINIMUM ("bench <name> <ns/op>"), after one line per
// rep ("rep <name> <i> <ns/op>"). Checksums go to STDOUT once per row
// ("checksum <name> <value>", from the first rep), so stdout is deterministic
// and can be diffed between runs and against HotSpot. Args: [reps].
// Run `--compatible`, interleaving A/B binaries / switch settings.
//
//   bound-getter-invoke    - `findGetter(Box, v).bindTo(box)` through
//                            `invoke()`: the door's bound GETTER arm, which
//                            resolved the slot by NAME per call before
//                            `CRATONVM_MH_BOUND_FIELD_OWNER` (by the owner's
//                            id with it; no lookup at all with
//                            `CRATONVM_MH_FIELD_SLOT_FROM_MINT=1` too)
//   bound-setter-invoke    - `findSetter(Box, v).bindTo(box)` through `invoke(3)`
//   bound-getter-exact     - the bound getter through `invokeExact()` (a bound
//                            handle is never linked: the door)
//   poly-getter-exact      - `findGetter(Base, v)` through `invokeExact`, the
//                            receiver rotating over four subclasses:
//                            `CRATONVM_MH_FIELD_LINK_OWNER_ADMITS=1` keeps the
//                            link; without it every rotation is a door call
//                            (and after seven relinks, the door for good)
//   poly-getter-exact-obj  - the same through an `asType((Object)int)` copy
//   mono-getter-exact      - the getter at one receiver class (control: the
//                            link either way)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum bound-getter-invoke 600000
// checksum bound-setter-invoke 200000
// checksum bound-getter-exact 600000
// checksum poly-getter-exact 200000
// checksum poly-getter-exact-obj 200000
// checksum mono-getter-exact 200000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W65FieldSlotBench {
    public static class Box {
        public int v = 3;
    }

    public static class Base {
        public int v = 1;
    }

    public static class S1 extends Base {
    }

    public static class S2 extends Base {
        public int v = 2;
    }

    public static class S3 extends S1 {
    }

    public static class S4 extends S2 {
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
        MethodHandle bget = lk.findGetter(Box.class, "v", int.class).bindTo(box);
        MethodHandle bset = lk.findSetter(Box.class, "v", int.class).bindTo(box);
        MethodHandle pget = lk.findGetter(Base.class, "v", int.class);
        MethodHandle pgetObj = pget.asType(MethodType.methodType(int.class, Object.class));
        MethodHandle mget = lk.findGetter(Base.class, "v", int.class);
        Base[] ring = {new S1(), new S2(), new S3(), new S4()};
        Object[] ringObj = ring.clone();
        Base mono = new S3();
        int n = 200000;
        bench("bound-getter-invoke", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bget.invoke();
            return s;
        });
        bench("bound-setter-invoke", reps, n, k -> {
            for (int i = 0; i < k; i++) bset.invoke(3);
            return k;
        });
        bench("bound-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) bget.invokeExact();
            return s;
        });
        bench("poly-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) pget.invokeExact(ring[i & 3]);
            return s;
        });
        bench("poly-getter-exact-obj", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) pgetObj.invokeExact(ringObj[i & 3]);
            return s;
        });
        bench("mono-getter-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) mget.invokeExact(mono);
            return s;
        });
    }
}
