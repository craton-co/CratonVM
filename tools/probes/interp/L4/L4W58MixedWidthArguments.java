// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L4 (review): a `(long,long)long` /
// `(double,double)double` handle (Groovy 5's `IndyMath.plus` shape) adapted
// to reference parameters and called with a box of a NARROWER primitive
// (`Integer` into `long`), through every door, a `guardWithTest` and a
// `MutableCallSite`. HotSpot unboxes the `Integer` and widens it. CratonVM
// (wave 57) had two defects here:
// * a `guardWithTest` over the `asType`d target reported `(Object,Object)long`
//   as its `type()` (the leaf's return), so `MutableCallSite.setTarget`
//   refused it with `WrongMethodTypeException` (`gwt-type`, `callsite-*`;
//   `CRATONVM_MH_GUARD_TYPE_IS_TARGETS`);
// * ONE stamp over an adapter -- Groovy 5's exact chain for `def a = 5L;
//   a += 1`, `explicitCastArguments(filterArguments(plus, 1,
//   Number.longValue), (Object,int)Object)` -- handed the filter a raw `int`
//   as its `Number` receiver and answered `5` (`groovy-chain-*`,
//   `groovy-astype-*`, `filtered-explicit-ji`;
//   `CRATONVM_MH_ADAPTER_STAMP_CONVERTS`). Page
//   `docs/internal/fixed-bugs/interpreter-L4-groovy-mixed-width-arithmetic-answers-the-left-operand-FIXED-20261022.md`.
//
// Run: javac -d out L4W58MixedWidthArguments.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W58MixedWidthArguments
//
// Expected HotSpot 25 output: see the block at the end of this file.
import java.lang.invoke.*;

public class L4W58MixedWidthArguments {
    static long plus(long a, long b) { return a + b; }
    static double dplus(double a, double b) { return a + b; }

    interface Body { Object run() throws Throwable; }

    static void row(String name, Body b) {
        String out;
        try { out = String.valueOf(b.run()); } catch (Throwable t) { out = t.getClass().getName() + ": " + t.getMessage(); }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle plus = lk.findStatic(L4W58MixedWidthArguments.class, "plus",
            MethodType.methodType(long.class, long.class, long.class));
        MethodHandle dplus = lk.findStatic(L4W58MixedWidthArguments.class, "dplus",
            MethodType.methodType(double.class, double.class, double.class));
        MethodType oo = MethodType.methodType(Object.class, Object.class, Object.class);
        MethodType li = MethodType.methodType(Object.class, Long.class, Integer.class);
        row("explicit-oo-first", () -> MethodHandles.explicitCastArguments(plus, oo).invoke((Object) 5L, (Object) 1));
        row("astype-li-invoke", () -> plus.asType(li).invoke(5L, 1));
        row("astype-li-exact", () -> (Object) plus.asType(li).invokeExact((Long) 5L, (Integer) 1));
        row("astype-oo-invoke", () -> plus.asType(oo).invoke((Object) 5L, (Object) 1));
        row("astype-oo-exact", () -> (Object) plus.asType(oo).invokeExact((Object) 5L, (Object) 1));
        row("explicit-oo-exact", () -> (Object) MethodHandles.explicitCastArguments(plus, oo).invokeExact((Object) 5L, (Object) 1));
        row("invoke-direct-integer", () -> (long) plus.invoke(5L, (Object) Integer.valueOf(1)));
        row("invoke-direct-short", () -> (long) plus.invoke((Object) Short.valueOf((short) 2), 5L));
        row("iwa-integer", () -> plus.invokeWithArguments(5L, 1));
        row("double-astype-oo", () -> dplus.asType(oo).invoke((Object) 1.5, (Object) 1));
        row("double-explicit-oo", () -> MethodHandles.explicitCastArguments(dplus, oo).invoke((Object) 1.5, (Object) 1));
        row("double-iwa", () -> dplus.invokeWithArguments(1.5, 1));
        MethodHandle bound = MethodHandles.insertArguments(plus.asType(oo), 0, 5L);
        row("insert-astype-integer", () -> bound.invoke((Object) 1));
        MethodHandle guarded = MethodHandles.guardWithTest(
            MethodHandles.dropArguments(MethodHandles.constant(boolean.class, true), 0, Object.class, Object.class),
            plus.asType(oo), MethodHandles.dropArguments(MethodHandles.constant(Object.class, -1L), 0, Object.class, Object.class));
        row("gwt-astype-integer", () -> guarded.invoke((Object) 5L, (Object) 1));
        MutableCallSite cs = new MutableCallSite(oo);
        row("gwt-type", () -> guarded.type());
        row("callsite-integer", () -> {
            cs.setTarget(guarded);
            return (Object) cs.dynamicInvoker().invokeExact((Object) 5L, (Object) 1);
        });
        MethodType oi = MethodType.methodType(Object.class, Object.class, int.class);
        MethodType ji = MethodType.methodType(Object.class, long.class, int.class);
        row("astype-oi-exact", () -> (Object) plus.asType(oi).invokeExact((Object) 5L, 1));
        row("explicit-oi-exact", () -> (Object) MethodHandles.explicitCastArguments(plus, oi).invokeExact((Object) 5L, 1));
        row("astype-ji-exact", () -> (Object) plus.asType(ji).invokeExact(5L, 1));
        row("astype-oi-invoke", () -> plus.asType(oi).invoke((Object) 5L, 1));
        row("double-astype-oi-exact", () -> (Object) dplus.asType(oi).invokeExact((Object) 1.5, 1));
        MutableCallSite cs2 = new MutableCallSite(oi);
        row("callsite-oi", () -> {
            cs2.setTarget(MethodHandles.guardWithTest(
                MethodHandles.dropArguments(MethodHandles.constant(boolean.class, true), 0, Object.class, int.class),
                plus.asType(oi), MethodHandles.dropArguments(MethodHandles.constant(Object.class, -1L), 0, Object.class, int.class)));
            return (Object) cs2.dynamicInvoker().invokeExact((Object) 5L, 1);
        });
        // Groovy's FIRST call of a site runs the selected handle through
        // `invokeWithArguments` (`IndyInterface.fromCache`).
        row("iwa-explicit-oo", () -> MethodHandles.explicitCastArguments(plus, oo).invokeWithArguments(5L, 1));
        row("iwa-astype-oo", () -> plus.asType(oo).invokeWithArguments(5L, 1));
        row("iwa-astype-li", () -> plus.asType(li).invokeWithArguments(5L, 1));
        row("iwa-gwt", () -> guarded.invokeWithArguments(5L, 1));
        row("iwa-gwt-array", () -> guarded.invokeWithArguments(new Object[] {5L, 1}));
        row("iwa-gwt-list", () -> guarded.invokeWithArguments(java.util.List.of(5L, 1)));
        row("iwa-astype-oi", () -> plus.asType(oi).invokeWithArguments(5L, 1));
        row("iwa-double-explicit", () -> MethodHandles.explicitCastArguments(dplus, oo).invokeWithArguments(1.5, 1));
        row("iwa-catch", () -> MethodHandles.catchException(plus.asType(oo), Throwable.class,
            MethodHandles.dropArguments(MethodHandles.constant(Object.class, -1L), 0, Throwable.class, Object.class, Object.class)).invokeWithArguments(5L, 1));
        // Groovy 5's exact chain for `def a = 5L; a += 1` (`-Dgroovy.indy.logging`):
        // `IndyMath.plus(long,long)`, a `Number.longValue` transformer at
        // position 1 (`correctCoerce`), then `explicitCastArguments` to the
        // site's `(Object,int)Object`; the first call through
        // `invokeWithArguments`.
        MethodHandle longValue = lk.findVirtual(Number.class, "longValue", MethodType.methodType(long.class));
        MethodHandle filtered = MethodHandles.filterArguments(plus, 1, longValue);
        MethodHandle groovy = MethodHandles.explicitCastArguments(filtered, oi);
        row("groovy-filtered-type", () -> filtered.type());
        row("groovy-chain-iwa", () -> groovy.invokeWithArguments(5L, 1));
        row("groovy-chain-exact", () -> (Object) groovy.invokeExact((Object) 5L, 1));
        row("groovy-chain-invoke", () -> groovy.invoke((Object) 5L, 1));
        row("groovy-astype-exact", () -> (Object) filtered.asType(oi).invokeExact((Object) 5L, 1));
        row("groovy-astype-invoke", () -> filtered.asType(oi).invoke((Object) 5L, 1));
        row("filtered-invoke-int", () -> (long) filtered.invoke(5L, 1));
        row("filtered-explicit-ji", () -> (Object) MethodHandles.explicitCastArguments(filtered, ji).invokeExact(5L, 1));
        row("groovy-filtered-invoke", () -> (long) filtered.invoke(5L, (Number) 1));
        MethodHandle doubleValue = lk.findVirtual(Number.class, "doubleValue", MethodType.methodType(double.class));
        MethodHandle dgroovy = MethodHandles.explicitCastArguments(MethodHandles.filterArguments(dplus, 1, doubleValue), oi);
        row("groovy-double-chain-iwa", () -> dgroovy.invokeWithArguments(1.5, 1));
        row("explicit-oo-again", () -> MethodHandles.explicitCastArguments(plus, oo).invoke((Object) 5L, (Object) 1));
    }
}




// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical):
//   explicit-oo-first: 6
//   astype-li-invoke: 6
//   astype-li-exact: 6
//   astype-oo-invoke: 6
//   astype-oo-exact: 6
//   explicit-oo-exact: 6
//   invoke-direct-integer: 6
//   invoke-direct-short: 7
//   iwa-integer: 6
//   double-astype-oo: 2.5
//   double-explicit-oo: 2.5
//   double-iwa: 2.5
//   insert-astype-integer: 6
//   gwt-astype-integer: 6
//   gwt-type: (Object,Object)Object
//   callsite-integer: 6
//   astype-oi-exact: 6
//   explicit-oi-exact: 6
//   astype-ji-exact: 6
//   astype-oi-invoke: 6
//   double-astype-oi-exact: 2.5
//   callsite-oi: 6
//   iwa-explicit-oo: 6
//   iwa-astype-oo: 6
//   iwa-astype-li: 6
//   iwa-gwt: 6
//   iwa-gwt-array: 6
//   iwa-gwt-list: 6
//   iwa-astype-oi: 6
//   iwa-double-explicit: 2.5
//   iwa-catch: 6
//   groovy-filtered-type: (long,Number)long
//   groovy-chain-iwa: 6
//   groovy-chain-exact: 6
//   groovy-chain-invoke: 6
//   groovy-astype-exact: 6
//   groovy-astype-invoke: 6
//   filtered-invoke-int: 6
//   filtered-explicit-ji: 6
//   groovy-filtered-invoke: 6
//   groovy-double-chain-iwa: 2.5
//   explicit-oo-again: 6
