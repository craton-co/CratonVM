// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L4: REAL JDK method handles whose
// `LambdaForm` has no pregenerated linker (`*$Holder`), so HotSpot spins a
// `LambdaForm$MH` class for it (`InvokerBytecodeGenerator.generateCustomizedCode`).
// Pages `docs/internal/fixed-bugs/interpreter-L4-a-real-handle-over-a-spun-lambdaform-cannot-run-in-the-default-world-FIXED-20261020.md`
// and its proposal `interpreter-L4-proposal-spin-the-lambdaforms-real-handles-need-FIXED-20261020.md`
// (switch `CRATONVM_IBG_SPIN_ON_MISS`). CratonVM before the switch (wave 55,
// --jdk-only): `java.lang.InternalError: ()void : DMH.invokeStatic=...` on
// every call row. The first call row is repeated at the end (cold-VM gaps).
// Positive control (`CRATONVM_DBG_MH_DISPATCH=1`): `[IBG_SPIN]
// generateCustomizedCode: java/lang/invoke/LambdaForm$MH/... ` lines.
//
// Run: javac -d out L4W56SpunLambdaForms.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W56SpunLambdaForms
//   (HotSpot: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out ...)
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   get-invoke: 4
//   get-exact: 4
//   get-iwa: 4
//   set-invoke: 11
//   cas-true: true 12
//   cas-false: false 12
//   getandadd: 12 15
//   long-get: 40
//   string-get: s
//   static-get: 99
//   exact-invoker-get: 4
//   exact-invoker-wrong: java.lang.invoke.WrongMethodTypeException
//   get-invoke-again: 4
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Method;

public class L4W56SpunLambdaForms {
    int i = 4;
    long l = 40L;
    String s = "s";
    static int st = 99;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static void rowClass(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    static MethodHandle vhInvoker(MethodType t, VarHandle.AccessMode mode, boolean exact) throws Throwable {
        Method inv = MethodType.class.getDeclaredMethod("invokers");
        inv.setAccessible(true);
        Object invokers = inv.invoke(t);
        Method m = invokers.getClass().getDeclaredMethod(
                exact ? "varHandleMethodExactInvoker" : "varHandleMethodInvoker", VarHandle.AccessMode.class);
        m.setAccessible(true);
        return (MethodHandle) m.invoke(invokers, mode);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> me = L4W56SpunLambdaForms.class;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        VarHandle vi = lookup.findVarHandle(me, "i", int.class);
        VarHandle vl = lookup.findVarHandle(me, "l", long.class);
        VarHandle vs = lookup.findVarHandle(me, "s", String.class);
        VarHandle vst = lookup.findStaticVarHandle(me, "st", int.class);
        MethodHandle getI = vhInvoker(MethodType.methodType(int.class, me), VarHandle.AccessMode.GET, false);
        MethodHandle setI = vhInvoker(MethodType.methodType(void.class, me, int.class), VarHandle.AccessMode.SET, false);
        MethodHandle casI = vhInvoker(MethodType.methodType(boolean.class, me, int.class, int.class),
                VarHandle.AccessMode.COMPARE_AND_SET, false);
        MethodHandle gaaI = vhInvoker(MethodType.methodType(int.class, me, int.class),
                VarHandle.AccessMode.GET_AND_ADD, false);
        MethodHandle getL = vhInvoker(MethodType.methodType(long.class, me), VarHandle.AccessMode.GET, false);
        MethodHandle getS = vhInvoker(MethodType.methodType(String.class, me), VarHandle.AccessMode.GET, false);
        MethodHandle getSt = vhInvoker(MethodType.methodType(int.class), VarHandle.AccessMode.GET, false);
        MethodHandle exactGetI = vhInvoker(MethodType.methodType(int.class, me), VarHandle.AccessMode.GET, true);

        L4W56SpunLambdaForms o = new L4W56SpunLambdaForms();
        row("get-invoke", () -> (int) getI.invoke(vi, o));
        row("get-exact", () -> (int) getI.invokeExact(vi, o));
        row("get-iwa", () -> getI.invokeWithArguments(vi, o));
        row("set-invoke", () -> {
            setI.invoke(vi, o, 11);
            return o.i;
        });
        row("cas-true", () -> (boolean) casI.invoke(vi, o, 11, 12) + " " + o.i);
        row("cas-false", () -> (boolean) casI.invoke(vi, o, 11, 13) + " " + o.i);
        row("getandadd", () -> (int) gaaI.invoke(vi, o, 3) + " " + o.i);
        row("long-get", () -> (long) getL.invoke(vl, new L4W56SpunLambdaForms()));
        row("string-get", () -> (String) getS.invoke(vs, new L4W56SpunLambdaForms()));
        row("static-get", () -> (int) getSt.invoke(vst));
        row("exact-invoker-get", () -> (int) exactGetI.invoke(vi, new L4W56SpunLambdaForms()));
        // The exact invoker refuses a VarHandle whose access type differs.
        rowClass("exact-invoker-wrong", () -> (int) exactGetI.invoke(vl, new L4W56SpunLambdaForms()));
        row("get-invoke-again", () -> (int) getI.invoke(vi, new L4W56SpunLambdaForms()));
    }
}
