// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L4: the door rows
// `CRATONVM_MH_FIELD_SLOT_FROM_MINT` (an instance field handle carries its
// slot from mint; on top of `CRATONVM_MH_OWNER_ID_AUTHORITY=1`) moves.
// Timings go to STDERR, one line per row ("bench <name> <ns/op>"); the
// checksums go to STDOUT ("checksum <name> <value>"), so stdout is
// deterministic and can be diffed between runs and against HotSpot. Args:
// [reps]. Interleave A/B (authority on, mint switch off vs `=1`,
// `--compatible`), medians of >= 5 reps.
//
//   getter-invoke           - `findGetter(Box, v)` through `invoke` at
//                             `(Object)int`: the door's instance GETTER arm,
//                             whose per-call slot lookup the switch skips
//   setter-invoke           - `findSetter(Box, v)` through `invoke`
//   unreflect-getter-invoke - `unreflectGetter(Box.v)` through `invoke`
//   astype-getter-invoke    - an `asType((Object)Object)` copy through
//                             `invoke` (the copy keeps the minted slot)
//   inherited-getter-invoke - a field declared four classes up, read through
//                             the leaf class's handle (the longest lookup walk)
//   getter-exact            - the getter through `invokeExact` at `(Box)int`:
//                             the field-accessor link (control)
//   static-getter-invoke    - `findStaticGetter` through `invoke` (control:
//                             no minted slot)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum getter-invoke 600000
// checksum setter-invoke 200000
// checksum unreflect-getter-invoke 600000
// checksum astype-getter-invoke 600000
// checksum inherited-getter-invoke 1400000
// checksum getter-exact 600000
// checksum static-getter-invoke 1000000
// HOTSPOT_EXPECTED_END
// (one block per rep; the block above is `reps = 1`.)
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W64FieldSlotBench {
    public static class Box {
        public static int s = 5;
        public int v = 3;
    }

    public static class L0 {
        public int deep = 7;
    }

    public static class L1 extends L0 {
        public int l1 = 1;
    }

    public static class L2 extends L1 {
        public int l2 = 2;
    }

    public static class L3 extends L2 {
        public int l3 = 3;
    }

    public static class L4 extends L3 {
        public int l4 = 4;
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
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle getter = lk.findGetter(Box.class, "v", int.class);
        MethodHandle setter = lk.findSetter(Box.class, "v", int.class);
        MethodHandle ugetter = lk.unreflectGetter(Box.class.getField("v"));
        MethodHandle agetter = getter.asType(MethodType.methodType(Object.class, Object.class));
        MethodHandle igetter = lk.findGetter(L4.class, "deep", int.class);
        MethodHandle sgetter = lk.findStaticGetter(Box.class, "s", int.class);
        Object box = new Box();
        Box typed = (Box) box;
        Object leaf = new L4();
        int n = 200000;
        for (int rep = 0; rep < reps; rep++) {
            bench("getter-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) getter.invoke(box);
                return s;
            });
            bench("setter-invoke", n, k -> {
                for (int i = 0; i < k; i++) setter.invoke(box, 3);
                return k;
            });
            bench("unreflect-getter-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) ugetter.invoke(box);
                return s;
            });
            bench("astype-getter-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (Integer) agetter.invoke(box);
                return s;
            });
            bench("inherited-getter-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) igetter.invoke(leaf);
                return s;
            });
            bench("getter-exact", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) getter.invokeExact(typed);
                return s;
            });
            bench("static-getter-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) sgetter.invoke();
                return s;
            });
        }
    }
}
