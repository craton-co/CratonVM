// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 132, lane L4 (page
// docs/known-issues/interpreter/gap-hidden-walks-lack-hotspots-spun-lambda-form-frames-20261016.md,
// residual 2; switch CRATONVM_MH_IWA_SERVED_CUSTOMIZATION, parity, default ON, =0 off).
//
// On HotSpot, MethodHandle.invokeWithArguments runs
// `genericMethodType(n).invokers().spreadInvoker(0).invokeExact(asType(generic), args)`. The spread
// invoker's form calls `Invokers.checkCustomized(adapter)` once per call, so the adapter (the
// asTypeCache entry for genericMethodType(n), or the handle itself when its type already is
// genericMethodType(n)) counts every call in `customizationCount` and, at the call after the count
// reached CUSTOMIZE_THRESHOLD (127), is customized (`form.customized == adapter`). CratonVM's served
// invokeWithArguments ran the target without that linker and never counted.
//
// What it prints (by reflection, --add-opens java.base/java.lang.invoke): after k calls through one
// handle, the adapter's customizationCount and whether its form is customized for it, for
//   static-int  (int)int static: the adapter is a BoundMethodHandle;
//   static-obj  (Object)Object static: the adapter is the handle itself (a DirectMethodHandle);
//   virtual     String.concat (String,String)String;
//   list        the List overload (counts once per call, as the array overload it calls);
//   throws      a target that throws (the count is taken before the target runs);
//   badarg      a String into an int parameter (ClassCastException in the adapter, after the count);
//   arity       a wrong argument count (asType's WrongMethodTypeException: no adapter, no count).
// With the argument `walk` it also prints the SHOW_HIDDEN_FRAMES walk of a customized static-obj
// call (diagnostic only: CratonVM's walk lists the pregenerated holder, see the page).
//
// Run: javac -d . L4W132IwaCustomization.java
//      java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp . L4W132IwaCustomization   (HotSpot; -Xint prints the same)
//      cratonvm --jdk-only [--nojit] --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp . L4W132IwaCustomization
//      cratonvm --compatible [--nojit] --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp . L4W132IwaCustomization
//      and each with CRATONVM_MH_IWA_SERVED_CUSTOMIZATION=0 (the old answer: every count 0, never customized).
//      Threshold arm: add -Djava.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD=3 (HotSpot: see the
//      THRESHOLD_3 block below); and =-1 (HotSpot: every count 0, never customized).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// static-int k=0 count=0 customized=false adapterIsHandle=false
// static-int k=1 count=1 customized=false adapterIsHandle=false
// static-int k=2 count=2 customized=false adapterIsHandle=false
// static-int k=126 count=126 customized=false adapterIsHandle=false
// static-int k=127 count=127 customized=false adapterIsHandle=false
// static-int k=128 count=127 customized=true adapterIsHandle=false
// static-int k=200 count=127 customized=true adapterIsHandle=false
// static-int result 7
// static-obj k=0 count=0 customized=false adapterIsHandle=true
// static-obj k=1 count=1 customized=false adapterIsHandle=true
// static-obj k=2 count=2 customized=false adapterIsHandle=true
// static-obj k=126 count=126 customized=false adapterIsHandle=true
// static-obj k=127 count=127 customized=false adapterIsHandle=true
// static-obj k=128 count=127 customized=true adapterIsHandle=true
// static-obj k=200 count=127 customized=true adapterIsHandle=true
// static-obj result x
// virtual k=0 count=0 customized=false adapterIsHandle=false
// virtual k=1 count=1 customized=false adapterIsHandle=false
// virtual k=2 count=2 customized=false adapterIsHandle=false
// virtual k=126 count=126 customized=false adapterIsHandle=false
// virtual k=127 count=127 customized=false adapterIsHandle=false
// virtual k=128 count=127 customized=true adapterIsHandle=false
// virtual k=200 count=127 customized=true adapterIsHandle=false
// virtual result ab
// list k=0 count=0 customized=false adapterIsHandle=false
// list k=1 count=1 customized=false adapterIsHandle=false
// list k=2 count=2 customized=false adapterIsHandle=false
// list k=126 count=126 customized=false adapterIsHandle=false
// list k=127 count=127 customized=false adapterIsHandle=false
// list k=128 count=127 customized=true adapterIsHandle=false
// list k=200 count=127 customized=true adapterIsHandle=false
// list result 7
// throws after 5 calls: count=5 customized=false adapterIsHandle=false thrown=5
// badarg after 5 calls: count=5 customized=false adapterIsHandle=false thrown=java.lang.ClassCastException
// arity after 5 calls: count=0 customized=false adapterIsHandle=false thrown=java.lang.invoke.WrongMethodTypeException
// HOTSPOT_EXPECTED_END
//
// With the argument `walk` HotSpot adds (diagnostic, not compared; the customized DirectMethodHandle
// runs a spun LambdaForm$DMH instead of the pregenerated holder):
//   walk cold [java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.LambdaForm$MH/ADDR::invoke]
//   walk customized [java.lang.invoke.LambdaForm$DMH/ADDR::invokeStatic, java.lang.invoke.LambdaForm$MH/ADDR::invoke] count=127 customized=true adapterIsHandle=true
//
// THRESHOLD_3 (HotSpot 25.0.3, -Djava.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD=3, measured):
// k=0/1/2 rows read count=k customized=false; every k=126/127/128/200 row reads count=3
// customized=true; throws and badarg: count=3 customized=true; arity: count=0 customized=false.
// THRESHOLD_-1 (=-1, measured): every row count=0 customized=false. Read, not compared.

import java.lang.StackWalker.Option;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;
import java.util.EnumSet;
import java.util.List;

public class L4W132IwaCustomization {
    static final Field FORM;
    static final Field CUSTOMIZED;
    static final Field COUNT;

    static {
        try {
            FORM = MethodHandle.class.getDeclaredField("form");
            FORM.setAccessible(true);
            Class<?> lf = Class.forName("java.lang.invoke.LambdaForm");
            CUSTOMIZED = lf.getDeclaredField("customized");
            CUSTOMIZED.setAccessible(true);
            COUNT = MethodHandle.class.getDeclaredField("customizationCount");
            COUNT.setAccessible(true);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int tInt(int x) { return x; }
    static Object tObj(Object x) { return x; }
    static int calls;
    static int tThrow(int x) { calls++; throw new IllegalStateException("t" + x); }

    static String walked;
    static Object tWalk(Object x) {
        walked = StackWalker.getInstance(EnumSet.of(Option.SHOW_HIDDEN_FRAMES)).walk(s ->
                s.skip(1).limit(2).map(f -> f.getClassName().replaceAll("/0x[0-9a-fA-F]+", "/ADDR")
                        + "::" + f.getMethodName()).toList()).toString();
        return x;
    }

    static String state(MethodHandle h, int n) throws Exception {
        MethodHandle adapter = h.asType(MethodType.genericMethodType(n));
        Object form = FORM.get(adapter);
        Object customized = CUSTOMIZED.get(form);
        byte count = COUNT.getByte(adapter);
        return "count=" + count + " customized=" + (customized == adapter)
                + (customized != null && customized != adapter ? "(other)" : "")
                + " adapterIsHandle=" + (adapter == h);
    }

    interface Call { Object run() throws Throwable; }

    static void family(String name, MethodHandle h, int n, Call call) throws Throwable {
        int[] ks = {0, 1, 2, 126, 127, 128, 200};
        int done = 0;
        Object last = null;
        for (int k : ks) {
            while (done < k) {
                last = call.run();
                done++;
            }
            System.out.println(name + " k=" + k + " " + state(h, n));
        }
        System.out.println(name + " result " + last);
    }

    static void failing(String name, MethodHandle h, int n, Call call) throws Exception {
        String thrown = "none";
        for (int i = 0; i < 5; i++) {
            try {
                call.run();
            } catch (IllegalStateException e) {
                thrown = String.valueOf(calls);
            } catch (Throwable t) {
                thrown = t.getClass().getName();
            }
        }
        System.out.println(name + " after 5 calls: " + state(h, n) + " thrown=" + thrown);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W132IwaCustomization.class;
        MethodHandle hi = l.findStatic(me, "tInt", MethodType.methodType(int.class, int.class));
        MethodHandle ho = l.findStatic(me, "tObj", MethodType.methodType(Object.class, Object.class));
        MethodHandle hv = l.findVirtual(String.class, "concat", MethodType.methodType(String.class, String.class));
        MethodHandle hl = l.findStatic(me, "tInt", MethodType.methodType(int.class, int.class)).asType(
                MethodType.methodType(Integer.class, int.class));
        MethodHandle ht = l.findStatic(me, "tThrow", MethodType.methodType(int.class, int.class));
        MethodHandle hb = l.findStatic(me, "tInt", MethodType.methodType(int.class, int.class))
                .asType(MethodType.methodType(long.class, int.class));
        MethodHandle ha = l.findStatic(me, "tObj", MethodType.methodType(Object.class, Object.class))
                .asType(MethodType.methodType(String.class, Object.class));
        Integer seven = 7;
        family("static-int", hi, 1, () -> hi.invokeWithArguments(seven));
        family("static-obj", ho, 1, () -> ho.invokeWithArguments("x"));
        family("virtual", hv, 2, () -> hv.invokeWithArguments("a", "b"));
        List<Object> one = List.of(seven);
        family("list", hl, 1, () -> hl.invokeWithArguments(one));
        failing("throws", ht, 1, () -> ht.invokeWithArguments(seven));
        failing("badarg", hb, 1, () -> hb.invokeWithArguments("no"));
        failing("arity", ha, 1, () -> ha.invokeWithArguments("p", "q"));
        if (args.length > 0 && args[0].equals("walk")) {
            MethodHandle hw = l.findStatic(me, "tWalk", MethodType.methodType(Object.class, Object.class));
            hw.invokeWithArguments("w");
            System.out.println("walk cold " + walked);
            for (int i = 0; i < 200; i++) {
                hw.invokeWithArguments("w");
            }
            System.out.println("walk customized " + walked + " " + state(hw, 1));
        }
    }
}
