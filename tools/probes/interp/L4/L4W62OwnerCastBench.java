// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L4: the door shapes
// `CRATONVM_MH_INSTANCE_OWNER_ID` (an unbound instance handle's receiver cast
// answered by its owner's class id) touches, us/op, one line per row ("bench
// <name> <us/op>"), for interleaved A/B runs (medians of >= 5 reps). Args:
// [reps].
//
//   virtual-invoke     - `findVirtual(Box, get)` through `invoke` at an
//                        `(Object)int` site: the leaf casts the receiver
//   virtual-exact      - the same handle through `invokeExact` at its own type
//   sub-invoke         - the same handle, a `Sub` receiver (the subclass arm)
//   getter-invoke      - `findGetter(Box, v)` through `invoke` at `(Object)int`
//   proxy-invoke       - a lambda proxy's `run` through `invoke` at `(Object)void`
//   bindTo             - `findVirtual(Box, get).bindTo(box)`
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W62OwnerCastBench {
    static int sink;

    public static class Box {
        public int v = 3;

        public int get() {
            return v;
        }
    }

    public static class Sub extends Box {
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static void bench(String name, int n, Body b) throws Throwable {
        b.run(Math.max(1, n / 10));
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        sink += (int) s;
        System.out.printf(java.util.Locale.ROOT, "bench %s %.4f%n", name, (t1 - t0) / 1000.0 / n);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle get = lk.findVirtual(Box.class, "get", MethodType.methodType(int.class));
        MethodHandle field = lk.findGetter(Box.class, "v", int.class);
        Runnable r = () -> {};
        MethodHandle run = MethodHandles.privateLookupIn(r.getClass(), lk)
                .findVirtual(r.getClass(), "run", MethodType.methodType(void.class));
        Object box = new Box();
        Box typed = (Box) box;
        Object sub = new Sub();
        Object proxy = r;
        for (int rep = 0; rep < reps; rep++) {
            bench("virtual-invoke", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) get.invoke(box);
                return s;
            });
            bench("virtual-exact", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) get.invokeExact(typed);
                return s;
            });
            bench("sub-invoke", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) get.invoke(sub);
                return s;
            });
            bench("getter-invoke", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) field.invoke(box);
                return s;
            });
            bench("proxy-invoke", 200000, n -> {
                for (int i = 0; i < n; i++) run.invoke(proxy);
                return n;
            });
            bench("bindTo", 50000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += get.bindTo(typed).type().parameterCount();
                return s;
            });
        }
    }
}
