// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 127, lane L4: the float / double atomics --
// getAndSet, getAndAdd and compareAndExchange (plain, Acquire, Release) --
// through a field / static / array-element VarHandle, through
// `VarHandle.toMethodHandle` (invokeExact and invokeWithArguments), through
// `sun.misc.Unsafe`'s int / long atomics over a float / double slot, and
// through `jdk.internal.misc.Unsafe`'s own `getAndSetFloat` /
// `getAndAddFloat` / `compareAndExchangeFloat` (and the double twins) by
// reflection. Each row prints the returned value and the variable after it.
//
// The JDK's float atomics are int atomics over the raw bits
// (`getAndSetFloat` loops `intBitsToFloat(getIntVolatile)` /
// `weakCompareAndSetInt`; `compareAndExchangeFloat` is
// `compareAndExchangeInt(floatToRawIntBits(e), floatToRawIntBits(x))`).
// CratonVM's `getIntVolatile` answered a float slot's `Value::Float` to the
// `I` return (read as 0.0), and its `compareAndExchangeInt` never matched
// a float slot and answered `expected` having stored nothing: getAndSet
// returned 0.0, compareAndExchange "succeeded" without a store (probe
// `L4W123RealVarForm`, `tmh f GET_AND_SET`).
// docs/known-issues/interpreter/i127-L4-varhandle-float-get-and-set-returns-zero-20261228.md
//
// Rows -0.0 / NaN check the exchange compares raw bits (HotSpot's cmpxchg):
// -0.0 never matches 0.0, the canonical NaN matches itself. The `hot` rows
// repeat an op 20,000 times so the JIT sees them; they print sums only.
//
// Run: javac -d . L4W127FloatAtomics.java
//      java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp . L4W127FloatAtomics
//      java -Xint --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp . L4W127FloatAtomics
// CratonVM: the same `--add-exports` flag, in --jdk-only and --compatible,
// JIT and --nojit. Without the flag only the `internal` rows change (to
// `internal skipped ...`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// vh f getAndSet -> 1.5 now 2.5
// vh f getAndSetAcquire -> 2.5 now 3.0
// vh f getAndSetRelease -> 3.0 now 3.5
// vh f getAndAdd -> 3.5 now 4.75
// vh f getAndAddAcquire -> 4.75 now 5.25
// vh f getAndAddRelease -> 5.25 now 3.25
// vh f compareAndExchange hit -> 3.25 now 4.0
// vh f compareAndExchange miss -> 4.0 now 4.0
// vh f compareAndExchangeAcquire hit -> 4.0 now 4.5
// vh f compareAndExchangeRelease hit -> 4.5 now 5.5
// vh f compareAndExchangeRelease miss -> 5.5 now 5.5
// vh f compareAndSet hit -> true now 6.0
// vh f compareAndSet miss -> false now 6.0
// vh f compareAndExchange -0.0 vs 0.0 -> 0.0 now 0.0
// vh f compareAndExchange NaN -> NaN now 9.0
// vh f getAndSet after NaN -> 9.0 now -1.0
// vh d getAndSet -> 2.5 now 3.5
// vh d getAndSetAcquire -> 3.5 now 4.0
// vh d getAndSetRelease -> 4.0 now 4.5
// vh d getAndAdd -> 4.5 now 5.75
// vh d getAndAddAcquire -> 5.75 now 6.25
// vh d getAndAddRelease -> 6.25 now 4.25
// vh d compareAndExchange hit -> 4.25 now 7.0
// vh d compareAndExchange miss -> 7.0 now 7.0
// vh d compareAndExchangeAcquire hit -> 7.0 now 7.5
// vh d compareAndExchangeRelease hit -> 7.5 now 8.5
// vh d compareAndExchangeRelease miss -> 8.5 now 8.5
// vh d compareAndSet hit -> true now 9.0
// vh d compareAndSet miss -> false now 9.0
// vh d compareAndExchange -0.0 vs 0.0 -> 0.0 now 0.0
// vh d compareAndExchange NaN -> NaN now 9.0
// vh d getAndSet after NaN -> 9.0 now -1.0
// vh SF getAndSet -> 1.5 now 2.5
// vh SF getAndAdd -> 2.5 now 4.0
// vh SF compareAndExchange hit -> 4.0 now 6.0
// vh SF compareAndExchange miss -> 6.0 now 6.0
// vh SD getAndSet -> 2.5 now 3.5
// vh SD getAndAdd -> 3.5 now 5.0
// vh SD compareAndExchange hit -> 5.0 now 6.0
// vh SD compareAndExchange miss -> 6.0 now 6.0
// vh fa getAndSet -> 2.5 now 4.0
// vh fa getAndAdd -> 4.0 now 4.5
// vh fa compareAndExchange hit -> 4.5 now 5.0
// vh fa compareAndExchange miss -> 5.0 now 5.0
// vh fa neighbours -> 1.5 now 3.5
// vh da getAndSet -> 2.5 now 4.0
// vh da getAndAdd -> 4.0 now 4.5
// vh da compareAndExchange hit -> 4.5 now 5.0
// vh da compareAndExchange miss -> 5.0 now 5.0
// vh da neighbours -> 1.5 now 3.5
// tmh f GET_AND_SET exact -> 1.5 now 2.5
// tmh f GET_AND_ADD exact -> 2.5 now 4.0
// tmh f COMPARE_AND_EXCHANGE exact -> 4.0 now 2.5
// tmh f GET_AND_SET args -> 2.5 now 4.0
// tmh f GET_AND_ADD args -> 4.0 now 6.5
// tmh f COMPARE_AND_EXCHANGE args -> 6.5 now 1.0
// tmh f GET_AND_SET_ACQUIRE args -> 1.0 now 2.0
// tmh f GET_AND_ADD_RELEASE args -> 2.0 now 4.0
// tmh d GET_AND_SET exact -> 2.5 now 3.5
// tmh d GET_AND_ADD exact -> 3.5 now 5.0
// tmh d COMPARE_AND_EXCHANGE exact -> 5.0 now 2.5
// tmh d GET_AND_SET args -> 2.5 now 7.0
// tmh d GET_AND_ADD args -> 7.0 now 9.5
// tmh d COMPARE_AND_EXCHANGE args -> 9.5 now 1.0
// sun f getIntVolatile bits ok true
// sun f getAndSetInt -> 1.5 now 2.5
// sun f compareAndSwapInt hit -> true now 3.5
// sun f compareAndSwapInt miss -> false now 3.5
// sun d getLongVolatile bits ok true
// sun d getAndSetLong -> 2.5 now 3.5
// sun d compareAndSwapLong hit -> true now 4.5
// sun d compareAndSwapLong miss -> false now 4.5
// sun fa getAndSetInt -> 2.5 now 6.0
// sun fa neighbour -> 1.5 now 1.5
// sun da getAndSetLong -> 2.5 now 6.0
// sun da neighbour -> 1.5 now 1.5
// internal f getAndSetFloat -> 1.5 now 2.5
// internal f getAndAddFloat -> 2.5 now 4.0
// internal f compareAndExchangeFloat hit -> 4.0 now 5.0
// internal f compareAndExchangeFloat miss -> 5.0 now 5.0
// internal d getAndSetDouble -> 2.5 now 3.5
// internal d getAndAddDouble -> 3.5 now 5.0
// internal d compareAndExchangeDouble hit -> 5.0 now 6.0
// internal d compareAndExchangeDouble miss -> 6.0 now 6.0
// internal fa getAndSetFloat -> 2.5 now 4.0
// internal fa getAndAddFloat -> 4.0 now 4.5
// internal fa compareAndExchangeFloat hit -> 4.5 now 5.0
// internal fa compareAndExchangeFloat miss -> 5.0 now 5.0
// internal fa neighbours -> 1.5 now 3.5
// internal da getAndSetDouble -> 2.5 now 4.0
// internal da getAndAddDouble -> 4.0 now 4.5
// internal da compareAndExchangeDouble hit -> 4.5 now 5.0
// internal da compareAndExchangeDouble miss -> 5.0 now 5.0
// internal da neighbours -> 1.5 now 3.5
// hot vh f getAndAdd sum 1.9999E8 final 20000.0
// hot vh d getAndSet sum 1.9999E8 final 20000.0
// hot tmh f compareAndExchange final 20000.0 misses 0
// hot vh fa getAndAdd 2500.0 2500.0 2500.0 2500.0
// done
// HOTSPOT_EXPECTED_END
//
// The orchestrator compares stdout against the block above, minus lines
// starting with TIME (none here). stderr carries HotSpot's sun.misc.Unsafe
// deprecation warning and is not compared.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class L4W127FloatAtomics {

    static class Box {
        float f = 1.5f;
        double d = 2.5;
        static float SF = 1.5f;
        static double SD = 2.5;
    }

    static void p(String row, float r, float now) {
        System.out.println(row + " -> " + r + " now " + now);
    }

    static void p(String row, double r, double now) {
        System.out.println(row + " -> " + r + " now " + now);
    }

    static void p(String row, boolean r, float now) {
        System.out.println(row + " -> " + r + " now " + now);
    }

    static void p(String row, boolean r, double now) {
        System.out.println(row + " -> " + r + " now " + now);
    }

    static void p(String row, Object r, Object now) {
        System.out.println(row + " -> " + r + " now " + now);
    }

    static void fieldFloat(MethodHandles.Lookup l) throws Throwable {
        VarHandle F = l.findVarHandle(Box.class, "f", float.class);
        Box b = new Box();
        p("vh f getAndSet", (float) F.getAndSet(b, 2.5f), b.f);
        p("vh f getAndSetAcquire", (float) F.getAndSetAcquire(b, 3.0f), b.f);
        p("vh f getAndSetRelease", (float) F.getAndSetRelease(b, 3.5f), b.f);
        p("vh f getAndAdd", (float) F.getAndAdd(b, 1.25f), b.f);
        p("vh f getAndAddAcquire", (float) F.getAndAddAcquire(b, 0.5f), b.f);
        p("vh f getAndAddRelease", (float) F.getAndAddRelease(b, -2.0f), b.f);
        p("vh f compareAndExchange hit", (float) F.compareAndExchange(b, 3.25f, 4.0f), b.f);
        p("vh f compareAndExchange miss", (float) F.compareAndExchange(b, 100.0f, 5.0f), b.f);
        p("vh f compareAndExchangeAcquire hit", (float) F.compareAndExchangeAcquire(b, 4.0f, 4.5f), b.f);
        p("vh f compareAndExchangeRelease hit", (float) F.compareAndExchangeRelease(b, 4.5f, 5.5f), b.f);
        p("vh f compareAndExchangeRelease miss", (float) F.compareAndExchangeRelease(b, 4.5f, 6.5f), b.f);
        p("vh f compareAndSet hit", (boolean) F.compareAndSet(b, 5.5f, 6.0f), b.f);
        p("vh f compareAndSet miss", (boolean) F.compareAndSet(b, 5.5f, 7.0f), b.f);
        b.f = 0.0f;
        p("vh f compareAndExchange -0.0 vs 0.0", (float) F.compareAndExchange(b, -0.0f, 8.0f), b.f);
        b.f = Float.NaN;
        p("vh f compareAndExchange NaN", (float) F.compareAndExchange(b, Float.NaN, 9.0f), b.f);
        p("vh f getAndSet after NaN", (float) F.getAndSet(b, -1.0f), b.f);
    }

    static void fieldDouble(MethodHandles.Lookup l) throws Throwable {
        VarHandle D = l.findVarHandle(Box.class, "d", double.class);
        Box b = new Box();
        p("vh d getAndSet", (double) D.getAndSet(b, 3.5), b.d);
        p("vh d getAndSetAcquire", (double) D.getAndSetAcquire(b, 4.0), b.d);
        p("vh d getAndSetRelease", (double) D.getAndSetRelease(b, 4.5), b.d);
        p("vh d getAndAdd", (double) D.getAndAdd(b, 1.25), b.d);
        p("vh d getAndAddAcquire", (double) D.getAndAddAcquire(b, 0.5), b.d);
        p("vh d getAndAddRelease", (double) D.getAndAddRelease(b, -2.0), b.d);
        p("vh d compareAndExchange hit", (double) D.compareAndExchange(b, 4.25, 7.0), b.d);
        p("vh d compareAndExchange miss", (double) D.compareAndExchange(b, 100.0, 5.0), b.d);
        p("vh d compareAndExchangeAcquire hit", (double) D.compareAndExchangeAcquire(b, 7.0, 7.5), b.d);
        p("vh d compareAndExchangeRelease hit", (double) D.compareAndExchangeRelease(b, 7.5, 8.5), b.d);
        p("vh d compareAndExchangeRelease miss", (double) D.compareAndExchangeRelease(b, 7.5, 9.5), b.d);
        p("vh d compareAndSet hit", (boolean) D.compareAndSet(b, 8.5, 9.0), b.d);
        p("vh d compareAndSet miss", (boolean) D.compareAndSet(b, 8.5, 10.0), b.d);
        b.d = 0.0;
        p("vh d compareAndExchange -0.0 vs 0.0", (double) D.compareAndExchange(b, -0.0, 8.0), b.d);
        b.d = Double.NaN;
        p("vh d compareAndExchange NaN", (double) D.compareAndExchange(b, Double.NaN, 9.0), b.d);
        p("vh d getAndSet after NaN", (double) D.getAndSet(b, -1.0), b.d);
    }

    static void statics(MethodHandles.Lookup l) throws Throwable {
        VarHandle SF = l.findStaticVarHandle(Box.class, "SF", float.class);
        VarHandle SD = l.findStaticVarHandle(Box.class, "SD", double.class);
        p("vh SF getAndSet", (float) SF.getAndSet(2.5f), Box.SF);
        p("vh SF getAndAdd", (float) SF.getAndAdd(1.5f), Box.SF);
        p("vh SF compareAndExchange hit", (float) SF.compareAndExchange(4.0f, 6.0f), Box.SF);
        p("vh SF compareAndExchange miss", (float) SF.compareAndExchange(4.0f, 7.0f), Box.SF);
        p("vh SD getAndSet", (double) SD.getAndSet(3.5), Box.SD);
        p("vh SD getAndAdd", (double) SD.getAndAdd(1.5), Box.SD);
        p("vh SD compareAndExchange hit", (double) SD.compareAndExchange(5.0, 6.0), Box.SD);
        p("vh SD compareAndExchange miss", (double) SD.compareAndExchange(5.0, 7.0), Box.SD);
    }

    static void arrays() throws Throwable {
        VarHandle FA = MethodHandles.arrayElementVarHandle(float[].class);
        VarHandle DA = MethodHandles.arrayElementVarHandle(double[].class);
        float[] fa = {1.5f, 2.5f, 3.5f};
        double[] da = {1.5, 2.5, 3.5};
        p("vh fa getAndSet", (float) FA.getAndSet(fa, 1, 4.0f), fa[1]);
        p("vh fa getAndAdd", (float) FA.getAndAdd(fa, 1, 0.5f), fa[1]);
        p("vh fa compareAndExchange hit", (float) FA.compareAndExchange(fa, 1, 4.5f, 5.0f), fa[1]);
        p("vh fa compareAndExchange miss", (float) FA.compareAndExchange(fa, 1, 4.5f, 6.0f), fa[1]);
        p("vh fa neighbours", fa[0], fa[2]);
        p("vh da getAndSet", (double) DA.getAndSet(da, 1, 4.0), da[1]);
        p("vh da getAndAdd", (double) DA.getAndAdd(da, 1, 0.5), da[1]);
        p("vh da compareAndExchange hit", (double) DA.compareAndExchange(da, 1, 4.5, 5.0), da[1]);
        p("vh da compareAndExchange miss", (double) DA.compareAndExchange(da, 1, 4.5, 6.0), da[1]);
        p("vh da neighbours", da[0], da[2]);
    }

    static void toMethodHandles(MethodHandles.Lookup l) throws Throwable {
        VarHandle F = l.findVarHandle(Box.class, "f", float.class);
        VarHandle D = l.findVarHandle(Box.class, "d", double.class);
        Box b = new Box();
        MethodHandle fGas = F.toMethodHandle(AccessMode.GET_AND_SET);
        MethodHandle fGaa = F.toMethodHandle(AccessMode.GET_AND_ADD);
        MethodHandle fCae = F.toMethodHandle(AccessMode.COMPARE_AND_EXCHANGE);
        p("tmh f GET_AND_SET exact", (float) fGas.invokeExact(b, 2.5f), b.f);
        p("tmh f GET_AND_ADD exact", (float) fGaa.invokeExact(b, 1.5f), b.f);
        p("tmh f COMPARE_AND_EXCHANGE exact", (float) fCae.invokeExact(b, 4.0f, 2.5f), b.f);
        p("tmh f GET_AND_SET args", fGas.invokeWithArguments(b, 4.0f), b.f);
        p("tmh f GET_AND_ADD args", fGaa.invokeWithArguments(b, 2.5f), b.f);
        p("tmh f COMPARE_AND_EXCHANGE args", fCae.invokeWithArguments(b, 6.5f, 1.0f), b.f);
        p("tmh f GET_AND_SET_ACQUIRE args",
                F.toMethodHandle(AccessMode.GET_AND_SET_ACQUIRE).invokeWithArguments(b, 2.0f), b.f);
        p("tmh f GET_AND_ADD_RELEASE args",
                F.toMethodHandle(AccessMode.GET_AND_ADD_RELEASE).invokeWithArguments(b, 2.0f), b.f);
        MethodHandle dGas = D.toMethodHandle(AccessMode.GET_AND_SET);
        MethodHandle dGaa = D.toMethodHandle(AccessMode.GET_AND_ADD);
        MethodHandle dCae = D.toMethodHandle(AccessMode.COMPARE_AND_EXCHANGE);
        p("tmh d GET_AND_SET exact", (double) dGas.invokeExact(b, 3.5), b.d);
        p("tmh d GET_AND_ADD exact", (double) dGaa.invokeExact(b, 1.5), b.d);
        p("tmh d COMPARE_AND_EXCHANGE exact", (double) dCae.invokeExact(b, 5.0, 2.5), b.d);
        p("tmh d GET_AND_SET args", dGas.invokeWithArguments(b, 7.0), b.d);
        p("tmh d GET_AND_ADD args", dGaa.invokeWithArguments(b, 2.5), b.d);
        p("tmh d COMPARE_AND_EXCHANGE args", dCae.invokeWithArguments(b, 9.5, 1.0), b.d);
    }

    static void sunMiscUnsafe() throws Throwable {
        Field tf = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        tf.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) tf.get(null);
        long fo = u.objectFieldOffset(Box.class.getDeclaredField("f"));
        long dof = u.objectFieldOffset(Box.class.getDeclaredField("d"));
        Box b = new Box();
        int fb = Float.floatToRawIntBits(1.5f);
        System.out.println("sun f getIntVolatile bits ok " + (u.getIntVolatile(b, fo) == fb));
        p("sun f getAndSetInt", Float.intBitsToFloat(u.getAndSetInt(b, fo, Float.floatToRawIntBits(2.5f))), b.f);
        p("sun f compareAndSwapInt hit",
                u.compareAndSwapInt(b, fo, Float.floatToRawIntBits(2.5f), Float.floatToRawIntBits(3.5f)), b.f);
        p("sun f compareAndSwapInt miss",
                u.compareAndSwapInt(b, fo, Float.floatToRawIntBits(2.5f), Float.floatToRawIntBits(4.5f)), b.f);
        long db = Double.doubleToRawLongBits(2.5);
        System.out.println("sun d getLongVolatile bits ok " + (u.getLongVolatile(b, dof) == db));
        p("sun d getAndSetLong", Double.longBitsToDouble(u.getAndSetLong(b, dof, Double.doubleToRawLongBits(3.5))), b.d);
        p("sun d compareAndSwapLong hit",
                u.compareAndSwapLong(b, dof, Double.doubleToRawLongBits(3.5), Double.doubleToRawLongBits(4.5)), b.d);
        p("sun d compareAndSwapLong miss",
                u.compareAndSwapLong(b, dof, Double.doubleToRawLongBits(3.5), Double.doubleToRawLongBits(5.5)), b.d);
        float[] fa = {1.5f, 2.5f};
        long fbase = u.arrayBaseOffset(float[].class);
        long fscale = u.arrayIndexScale(float[].class);
        p("sun fa getAndSetInt",
                Float.intBitsToFloat(u.getAndSetInt(fa, fbase + fscale, Float.floatToRawIntBits(6.0f))), fa[1]);
        p("sun fa neighbour", fa[0], fa[0]);
        double[] da = {1.5, 2.5};
        long dbase = u.arrayBaseOffset(double[].class);
        long dscale = u.arrayIndexScale(double[].class);
        p("sun da getAndSetLong",
                Double.longBitsToDouble(u.getAndSetLong(da, dbase + dscale, Double.doubleToRawLongBits(6.0))), da[1]);
        p("sun da neighbour", da[0], da[0]);
    }

    static void internalUnsafe() {
        Object u;
        Class<?> uc;
        try {
            uc = Class.forName("jdk.internal.misc.Unsafe");
            u = uc.getMethod("getUnsafe").invoke(null);
        } catch (Throwable t) {
            System.out.println("internal skipped " + t.getClass().getName());
            return;
        }
        try {
            Method ofo = uc.getMethod("objectFieldOffset", Class.class, String.class);
            long fo = (long) ofo.invoke(u, Box.class, "f");
            long dof = (long) ofo.invoke(u, Box.class, "d");
            Method gasF = uc.getMethod("getAndSetFloat", Object.class, long.class, float.class);
            Method gaaF = uc.getMethod("getAndAddFloat", Object.class, long.class, float.class);
            Method caeF = uc.getMethod("compareAndExchangeFloat", Object.class, long.class, float.class, float.class);
            Method gasD = uc.getMethod("getAndSetDouble", Object.class, long.class, double.class);
            Method gaaD = uc.getMethod("getAndAddDouble", Object.class, long.class, double.class);
            Method caeD = uc.getMethod("compareAndExchangeDouble", Object.class, long.class, double.class, double.class);
            Box b = new Box();
            p("internal f getAndSetFloat", gasF.invoke(u, b, fo, 2.5f), b.f);
            p("internal f getAndAddFloat", gaaF.invoke(u, b, fo, 1.5f), b.f);
            p("internal f compareAndExchangeFloat hit", caeF.invoke(u, b, fo, 4.0f, 5.0f), b.f);
            p("internal f compareAndExchangeFloat miss", caeF.invoke(u, b, fo, 4.0f, 6.0f), b.f);
            p("internal d getAndSetDouble", gasD.invoke(u, b, dof, 3.5), b.d);
            p("internal d getAndAddDouble", gaaD.invoke(u, b, dof, 1.5), b.d);
            p("internal d compareAndExchangeDouble hit", caeD.invoke(u, b, dof, 5.0, 6.0), b.d);
            p("internal d compareAndExchangeDouble miss", caeD.invoke(u, b, dof, 5.0, 7.0), b.d);
            Method abo = uc.getMethod("arrayBaseOffset", Class.class);
            Method ais = uc.getMethod("arrayIndexScale", Class.class);
            float[] fa = {1.5f, 2.5f, 3.5f};
            long fe = ((Number) abo.invoke(u, float[].class)).longValue() + ((Number) ais.invoke(u, float[].class)).longValue();
            p("internal fa getAndSetFloat", gasF.invoke(u, fa, fe, 4.0f), fa[1]);
            p("internal fa getAndAddFloat", gaaF.invoke(u, fa, fe, 0.5f), fa[1]);
            p("internal fa compareAndExchangeFloat hit", caeF.invoke(u, fa, fe, 4.5f, 5.0f), fa[1]);
            p("internal fa compareAndExchangeFloat miss", caeF.invoke(u, fa, fe, 4.5f, 6.0f), fa[1]);
            p("internal fa neighbours", fa[0], fa[2]);
            double[] da = {1.5, 2.5, 3.5};
            long de = ((Number) abo.invoke(u, double[].class)).longValue() + ((Number) ais.invoke(u, double[].class)).longValue();
            p("internal da getAndSetDouble", gasD.invoke(u, da, de, 4.0), da[1]);
            p("internal da getAndAddDouble", gaaD.invoke(u, da, de, 0.5), da[1]);
            p("internal da compareAndExchangeDouble hit", caeD.invoke(u, da, de, 4.5, 5.0), da[1]);
            p("internal da compareAndExchangeDouble miss", caeD.invoke(u, da, de, 4.5, 6.0), da[1]);
            p("internal da neighbours", da[0], da[2]);
        } catch (Throwable t) {
            Throwable c = t.getCause() != null ? t.getCause() : t;
            System.out.println("internal threw " + c.getClass().getName() + ": " + c.getMessage());
        }
    }

    static void hot(MethodHandles.Lookup l) throws Throwable {
        VarHandle F = l.findVarHandle(Box.class, "f", float.class);
        VarHandle D = l.findVarHandle(Box.class, "d", double.class);
        Box b = new Box();
        b.f = 0.0f;
        b.d = 0.0;
        double sumF = 0;
        double sumD = 0;
        for (int i = 0; i < 20000; i++) {
            sumF += (float) F.getAndAdd(b, 1.0f);
            sumD += (double) D.getAndSet(b, (double) (i + 1));
        }
        System.out.println("hot vh f getAndAdd sum " + sumF + " final " + b.f);
        System.out.println("hot vh d getAndSet sum " + sumD + " final " + b.d);
        MethodHandle cae = F.toMethodHandle(AccessMode.COMPARE_AND_EXCHANGE);
        b.f = 0.0f;
        int misses = 0;
        for (int i = 0; i < 20000; i++) {
            float cur = b.f;
            float w = (float) cae.invokeExact(b, cur, cur + 1.0f);
            if (Float.floatToRawIntBits(w) != Float.floatToRawIntBits(cur)) {
                misses++;
            }
        }
        System.out.println("hot tmh f compareAndExchange final " + b.f + " misses " + misses);
        float[] fa = new float[4];
        VarHandle FA = MethodHandles.arrayElementVarHandle(float[].class);
        for (int i = 0; i < 20000; i++) {
            float ignored = (float) FA.getAndAdd(fa, i & 3, 0.5f);
        }
        System.out.println("hot vh fa getAndAdd " + fa[0] + " " + fa[1] + " " + fa[2] + " " + fa[3]);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        fieldFloat(l);
        fieldDouble(l);
        statics(l);
        arrays();
        toMethodHandles(l);
        sunMiscUnsafe();
        internalUnsafe();
        hot(l);
        System.out.println("done");
    }
}
