// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L4 (review; proposal
// `i42-L4-proposal-native-adapters-carry-hotspots-type`): every
// `MethodHandles` combinator's `type()`, checked the way Groovy's and JRuby's
// call sites check it -- `MutableCallSite.setTarget` refuses a target whose
// `type()` is not the site's own (`WrongMethodTypeException`). Wave 58 fixed
// `guardWithTest` (`CRATONVM_MH_GUARD_TYPE_IS_TARGETS`); this sweeps the rest.
//
// Each row builds one adapter over `Object`-typed leaves with a primitive or
// narrower parameter somewhere, prints its `type()`, installs it in a
// `MutableCallSite` of the type HotSpot gives it, then installs its
// `asType` to a second site type, and calls both through `dynamicInvoker`
// (`invokeExact`) in a loop (counted in `long`). The first row is repeated
// last.
//
// Run: javac -d out L4W59CombinatorCallSiteTypes.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W59CombinatorCallSiteTypes
//
// Expected HotSpot 25 output: see the block at the end of this file.
import java.lang.invoke.*;
import java.util.function.Function;

public class L4W59CombinatorCallSiteTypes {
    static int twice(int x) { return x * 2; }
    static Object boxTwice(Object x) { return ((Integer) x) * 2; }
    static Object boxed(int x) { return x + 1; }
    static long widen(int x) { return x; }
    static int len(String s) { return s.length(); }
    static String str(int x) { return Integer.toString(x); }
    static boolean isEven(int x) { return (x & 1) == 0; }
    static int neg(int x) { return -x; }
    static int add(int a, int b) { return a + b; }
    static int sum3(int a, int b, int c) { return a + b + c; }
    static int fold(int a) { return a + 1; }
    static int boom(int x) { if (x >= 0) throw new IllegalStateException("x"); return x; }
    static int handler(IllegalStateException e, int x) { return x + 7; }
    static int cleanup(Throwable t, int r, int x) { return r + x; }
    static int sumArr(int[] a) { int s = 0; for (int v : a) s += v; return s; }

    interface Build { MethodHandle build(MethodHandles.Lookup lk) throws Throwable; }

    static MethodHandle st(MethodHandles.Lookup lk, String n, Class<?> r, Class<?>... p) throws Throwable {
        return lk.findStatic(L4W59CombinatorCallSiteTypes.class, n, MethodType.methodType(r, p));
    }

    static void row(String name, Build b, MethodType expected, MethodType retyped, Object... args) {
        String out;
        try {
            MethodHandle h = b.build(MethodHandles.lookup());
            StringBuilder s = new StringBuilder(String.valueOf(h.type()));
            MutableCallSite cs = new MutableCallSite(expected);
            try {
                cs.setTarget(h);
                s.append(" set=ok");
            } catch (Throwable t) {
                s.append(" set=").append(t.getClass().getSimpleName());
            }
            MutableCallSite cs2 = new MutableCallSite(retyped);
            try {
                cs2.setTarget(h.asType(retyped));
                int n = retyped.parameterCount();
                MethodHandle inv = cs2.dynamicInvoker().asType(MethodType.genericMethodType(n)).asSpreader(Object[].class, n);
                long sum = 0;
                for (int i = 0; i < 2000; i++) {
                    Object r = (Object) inv.invokeExact(args);
                    sum += r == null ? 0 : ((Number) r).longValue();
                }
                s.append(" retyped=").append(sum);
            } catch (Throwable t) {
                s.append(" retyped=").append(t.getClass().getSimpleName());
            }
            out = s.toString();
        } catch (Throwable t) {
            out = "build " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static MethodType mt(Class<?> r, Class<?>... p) { return MethodType.methodType(r, p); }

    public static void main(String[] a) throws Throwable {
        Class<?> I = int.class, O = Object.class, J = long.class, Z = boolean.class, S = String.class;
        Integer three = 3;
        row("filterArguments", lk -> MethodHandles.filterArguments(st(lk, "twice", I, I), 0, st(lk, "len", I, S)),
            mt(I, S), mt(O, O), "abcd");
        row("filterReturnValue", lk -> MethodHandles.filterReturnValue(st(lk, "twice", I, I), st(lk, "widen", J, I)),
            mt(J, I), mt(O, O), three);
        row("dropArguments", lk -> MethodHandles.dropArguments(st(lk, "twice", I, I), 1, S, O),
            mt(I, I, S, O), mt(J, Integer.class, S, O), three, "s", null);
        row("insertArguments", lk -> MethodHandles.insertArguments(st(lk, "add", I, I, I), 0, 5),
            mt(I, I), mt(O, O), three);
        row("foldArguments", lk -> MethodHandles.foldArguments(st(lk, "add", I, I, I), st(lk, "fold", I, I)),
            mt(I, I), mt(O, O), three);
        row("collectArguments", lk -> MethodHandles.collectArguments(st(lk, "add", I, I, I), 1, st(lk, "len", I, S)),
            mt(I, I, S), mt(O, O, O), three, "abc");
        row("catchException", lk -> MethodHandles.catchException(st(lk, "boom", I, I), IllegalStateException.class,
            st(lk, "handler", I, IllegalStateException.class, I)), mt(I, I), mt(O, O), three);
        row("permuteArguments", lk -> MethodHandles.permuteArguments(st(lk, "sum3", I, I, I, I), mt(I, I, I), 0, 1, 0),
            mt(I, I, I), mt(O, O, O), three, 4);
        row("guardWithTest", lk -> MethodHandles.guardWithTest(st(lk, "isEven", Z, I), st(lk, "twice", I, I), st(lk, "neg", I, I)),
            mt(I, I), mt(O, O), three);
        row("tryFinally", lk -> MethodHandles.tryFinally(st(lk, "twice", I, I), st(lk, "cleanup", I, Throwable.class, I, I)),
            mt(I, I), mt(O, O), three);
        row("asCollector", lk -> st(lk, "sumArr", I, int[].class).asCollector(int[].class, 2),
            mt(I, I, I), mt(O, O, O), three, 5);
        row("asSpreader", lk -> st(lk, "add", I, I, I).asSpreader(Object[].class, 2),
            mt(I, Object[].class), mt(O, O), (Object) new Object[] {1, 2});
        row("dropReturn", lk -> MethodHandles.dropReturn(st(lk, "twice", I, I)),
            mt(void.class, I), mt(void.class, O), three);
        row("filterArguments-boxed", lk -> MethodHandles.filterArguments(st(lk, "boxTwice", O, O), 0, st(lk, "boxed", O, I)),
            mt(O, I), mt(O, Integer.class), three);
        row("explicitCast-over-filter", lk -> MethodHandles.explicitCastArguments(
                MethodHandles.filterArguments(st(lk, "add", I, I, I), 1, st(lk, "twice", I, I)), mt(O, O, J)),
            mt(O, O, J), mt(O, O, O), three, 6L);
        row("countedLoop", lk -> MethodHandles.countedLoop(MethodHandles.constant(I, 4), null,
                MethodHandles.dropArguments(st(lk, "twice", I, I), 1, I)),
            mt(I), mt(O));
        MethodHandles.Lookup lk0 = MethodHandles.lookup();
        MutableCallSite fresh = new MutableCallSite(mt(O, O));
        System.out.println("uninitialized-site: " + fresh.type() + " " + fresh.getTarget().type() + " " + fresh.dynamicInvoker().type());
        fresh.setTarget(MethodHandles.insertArguments(st(lk0, "add", I, I, I), 0, 5).asType(mt(O, O)));
        System.out.println("dynamic-invoker-type: " + fresh.dynamicInvoker().type());
        System.out.println("dropReturn-invoke: " + (Object) MethodHandles.dropReturn(st(lk0, "twice", I, I)).invoke(3));
        row("filterArguments-again", lk -> MethodHandles.filterArguments(st(lk, "twice", I, I), 0, st(lk, "len", I, S)),
            mt(I, S), mt(O, O), "abcd");
    }
}

// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical):
//   filterArguments: (String)int set=ok retyped=16000
//   filterReturnValue: (int)long set=ok retyped=12000
//   dropArguments: (int,String,Object)int set=ok retyped=12000
//   insertArguments: (int)int set=ok retyped=16000
//   foldArguments: (int)int set=ok retyped=14000
//   collectArguments: (int,String)int set=ok retyped=12000
//   catchException: (int)int set=ok retyped=20000
//   permuteArguments: (int,int)int set=ok retyped=20000
//   guardWithTest: (int)int set=ok retyped=-6000
//   tryFinally: (int)int set=ok retyped=18000
//   asCollector: (int,int)int set=ok retyped=16000
//   asSpreader: (Object[])int set=ok retyped=6000
//   dropReturn: (int)void set=ok retyped=0
//   filterArguments-boxed: (int)Object set=ok retyped=16000
//   explicitCast-over-filter: (Object,long)Object set=ok retyped=30000
//   countedLoop: ()int set=ok retyped=0
//   uninitialized-site: (Object)Object (Object)Object (Object)Object
//   dynamic-invoker-type: (Object)Object
//   dropReturn-invoke: null
//   filterArguments-again: (String)int set=ok retyped=16000
