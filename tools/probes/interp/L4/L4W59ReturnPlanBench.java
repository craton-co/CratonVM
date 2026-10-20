// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L4: the door shapes
// `CRATONVM_MH_RETURN_PLAN` (a handle's declared type and parsed cast chain
// memoised per handle) touches, us/op, one line per row ("bench <name>
// <us/op>"), for interleaved A/B runs (medians of >= 5 reps). Args: [reps].
//
//   chain-exact    - Groovy 5's mixed-width shape: `explicitCastArguments(
//                    filterArguments(plus(long,long), 1, Number.longValue),
//                    (Object,int)Object)` through `invokeExact` (a stamp
//                    over an adapter: its chain is read and parsed per call)
//   callsite-exact - a `MutableCallSite`'s `dynamicInvoker().invokeExact` over
//                    `insertArguments(add, 0, 5).asType((Object)Object)`
//   astype-iwa     - `invokeWithArguments` of an `asType` copy
//   astype-invoke  - `invoke` of an `asType` copy at a primitive site
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;

public class L4W59ReturnPlanBench {
    static int sink;

    static long plus(long a, long b) { return a + b; }
    static int add(int a, int b) { return a + b; }
    static Object id(Object o) { return o; }

    interface Body { long run(int n) throws Throwable; }

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
        Class<?> me = L4W59ReturnPlanBench.class;
        MethodHandle plus = lk.findStatic(me, "plus", MethodType.methodType(long.class, long.class, long.class));
        MethodHandle longValue = lk.findVirtual(Number.class, "longValue", MethodType.methodType(long.class));
        MethodHandle chain = MethodHandles.explicitCastArguments(
                MethodHandles.filterArguments(plus, 1, longValue),
                MethodType.methodType(Object.class, Object.class, int.class));
        MethodHandle add = lk.findStatic(me, "add", MethodType.methodType(int.class, int.class, int.class));
        MethodType oo = MethodType.methodType(Object.class, Object.class);
        MutableCallSite cs = new MutableCallSite(oo);
        cs.setTarget(MethodHandles.insertArguments(add, 0, 5).asType(oo));
        MethodHandle invoker = cs.dynamicInvoker();
        MethodHandle idh = lk.findStatic(me, "id", oo).asType(MethodType.methodType(Object.class, Integer.class));
        MethodHandle astype = add.asType(MethodType.methodType(Object.class, Object.class, Object.class));
        Long five = 5L;
        Integer three = 3;
        int n = 20000;
        for (int r = 0; r < reps; r++) {
            bench("chain-exact", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (Long) (Object) chain.invokeExact((Object) five, i);
                return s;
            });
            bench("callsite-exact", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (Integer) (Object) invoker.invokeExact((Object) three);
                return s;
            });
            bench("astype-iwa", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (Integer) idh.invokeWithArguments(three);
                return s;
            });
            bench("astype-invoke", n, k -> {
                long s = 0;
                for (int i = 0; i < k; i++) s += (int) astype.invoke((Object) three, (Object) three);
                return s;
            });
        }
        System.out.println("sink " + (sink != 0));
    }
}
