// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L4: a REAL JDK `MethodHandle` (one the
// JDK's own `java.lang.invoke` code builds, not CratonVM's synthetic layout)
// invoked through the public doors with arguments and returns that need
// `asType`'s conversions: a boxed or narrower argument at a primitive
// parameter, a primitive result into an `Object` / wider site, the refusals
// `asType` raises, and a real handle reached through `invokeBasic` (the JDK's
// `basicInvoker`). Page
// `docs/internal/fixed-bugs/interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020.md`.
// CratonVM (wave 53, code read): the argument rows raise
// `InternalError: CratonVM cannot invoke a ...`, `basic-invoker` reached
// `invokeBasic` on the real handle, whose native fallback answered `null`.
// Positive controls (`CRATONVM_DBG_MH_DISPATCH=1`): `[MH_FOREIGN] running ...
// (converting from ...)` for the argument rows, and, with
// `CRATONVM_MH_FOREIGN_DMH_BY_MEMBER` on, `[MH_FOREIGN] dispatching
// DirectMethodHandle... by its member` for the `direct-static-*` /
// `direct-virtual-*` rows (never for `direct-lazy`, whose form is
// `invokeStaticInit`).
//
// Run: javac -d out L4W54RealJdkHandleArgs.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W54RealJdkHandleArgs
//   (HotSpot: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out ...)
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   set-int-boxed: 7
//   set-long-widened: 5
//   set-int-iwa: null 9
//   get-int-as-object: 4 Integer
//   get-int-as-long: 4
//   get-char-as-object: q Character
//   get-char-iwa: q Character
//   set-int-object-string: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   set-int-string: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W54RealJdkHandleArgs,int)void to (VarHandle,L4W54RealJdkHandleArgs,String)void
//   set-int-long: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W54RealJdkHandleArgs,int)void to (VarHandle,L4W54RealJdkHandleArgs,long)void
//   get-int-iwa-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W54RealJdkHandleArgs)int to (Object)Object
//   get-int-exact-long: java.lang.invoke.WrongMethodTypeException: handle's method type (VarHandle,L4W54RealJdkHandleArgs)int but found (VarHandle,L4W54RealJdkHandleArgs)long
//   get-int-null-handle: java.lang.NullPointerException
//   basic-invoker-type: (MethodHandle,Object,Object)int
//   basic-invoker: 4
//   generic-invoker: 4
//   direct-static-class: java.lang.invoke.DirectMethodHandle (int)int
//   direct-static-exact: 42
//   direct-static-boxed: 42
//   direct-static-iwa: 42
//   direct-virtual-exact: 7
//   direct-virtual-iwa: 7
//   direct-virtual-null: java.lang.NullPointerException
//   Lazy.<clinit>
//   direct-lazy: 1010
//   direct-static-exact-again: 42
//   astype-type: (VarHandle,Object)Object
//   astype-invoke-exact: 4 Integer
//   astype-receiver-type: (VarHandle,L4W54RealJdkHandleArgs)int
//   astype-receiver-exact: 4
//   bindto-type: (L4W54RealJdkHandleArgs)int
//   bindto-invoke: 4
//   bindto-receiver-type: (VarHandle,L4W54RealJdkHandleArgs)int
//   set-int-boxed-again: 7
//   set-long-widened-again: 5
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Method;

public class L4W54RealJdkHandleArgs {
    int i = 4;
    long l = 40L;
    char c = 'q';

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

    /** The class only: the message is the JDK body's helpful text. */
    static void rowClass(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    static Object invokers(MethodType t) throws Throwable {
        Method m = MethodType.class.getDeclaredMethod("invokers");
        m.setAccessible(true);
        return m.invoke(t);
    }

    static MethodHandle vhInvoker(MethodType t, VarHandle.AccessMode mode) throws Throwable {
        Object inv = invokers(t);
        Method m = inv.getClass().getDeclaredMethod("varHandleMethodInvoker", VarHandle.AccessMode.class);
        m.setAccessible(true);
        return (MethodHandle) m.invoke(inv, mode);
    }

    static MethodHandle invoker(MethodType t, String name) throws Throwable {
        Object inv = invokers(t);
        Method m = inv.getClass().getDeclaredMethod(name);
        m.setAccessible(true);
        return (MethodHandle) m.invoke(inv);
    }

    static int twice(int x) {
        return 2 * x;
    }

    int plus(int x) {
        return i + x;
    }

    static class Lazy {
        static boolean initialized;

        static {
            System.out.println("Lazy.<clinit>");
            initialized = true;
        }

        static int twice(int x) {
            return 2 * x + (initialized ? 1000 : 0);
        }
    }

    static MethodHandle realDirect(Method m) throws Throwable {
        Class<?> memberName = Class.forName("java.lang.invoke.MemberName");
        java.lang.reflect.Constructor<?> ctor = memberName.getDeclaredConstructor(Method.class);
        ctor.setAccessible(true);
        Object member = ctor.newInstance(m);
        Method make = Class.forName("java.lang.invoke.DirectMethodHandle").getDeclaredMethod("make", memberName);
        make.setAccessible(true);
        return (MethodHandle) make.invoke(null, member);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> me = L4W54RealJdkHandleArgs.class;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        VarHandle vi = lookup.findVarHandle(me, "i", int.class);
        VarHandle vl = lookup.findVarHandle(me, "l", long.class);
        VarHandle vc = lookup.findVarHandle(me, "c", char.class);
        MethodType getI = MethodType.methodType(int.class, me);
        MethodHandle hGetI = vhInvoker(getI, VarHandle.AccessMode.GET);
        MethodHandle hSetI = vhInvoker(MethodType.methodType(void.class, me, int.class), VarHandle.AccessMode.SET);
        MethodHandle hSetL = vhInvoker(MethodType.methodType(void.class, me, long.class), VarHandle.AccessMode.SET);
        MethodHandle hGetC = vhInvoker(MethodType.methodType(char.class, me), VarHandle.AccessMode.GET);

        row("set-int-boxed", () -> {
            L4W54RealJdkHandleArgs o = new L4W54RealJdkHandleArgs();
            hSetI.invoke(vi, o, (Object) Integer.valueOf(7));
            return o.i;
        });
        row("set-long-widened", () -> {
            L4W54RealJdkHandleArgs o = new L4W54RealJdkHandleArgs();
            hSetL.invoke(vl, o, 5);
            return o.l;
        });
        row("set-int-iwa", () -> {
            L4W54RealJdkHandleArgs o = new L4W54RealJdkHandleArgs();
            Object r = hSetI.invokeWithArguments(vi, o, 9);
            return r + " " + o.i;
        });
        row("get-int-as-object", () -> {
            Object r = hGetI.invoke(vi, new L4W54RealJdkHandleArgs());
            return r + " " + r.getClass().getSimpleName();
        });
        row("get-int-as-long", () -> (long) hGetI.invoke(vi, new L4W54RealJdkHandleArgs()));
        row("get-char-as-object", () -> {
            Object r = hGetC.invoke(vc, new L4W54RealJdkHandleArgs());
            return r + " " + r.getClass().getSimpleName();
        });
        row("get-char-iwa", () -> {
            Object r = hGetC.invokeWithArguments(vc, new L4W54RealJdkHandleArgs());
            return r + " " + r.getClass().getSimpleName();
        });
        row("set-int-object-string", () -> {
            hSetI.invoke(vi, new L4W54RealJdkHandleArgs(), (Object) "x");
            return "no-throw";
        });
        row("set-int-string", () -> {
            hSetI.invoke(vi, new L4W54RealJdkHandleArgs(), "x");
            return "no-throw";
        });
        row("set-int-long", () -> {
            hSetI.invoke(vi, new L4W54RealJdkHandleArgs(), 5L);
            return "no-throw";
        });
        row("get-int-iwa-short", () -> hGetI.invokeWithArguments(vi));
        row("get-int-exact-long", () -> (long) hGetI.invokeExact(vi, new L4W54RealJdkHandleArgs()));
        rowClass("get-int-null-handle", () -> (int) hGetI.invoke((VarHandle) null, new L4W54RealJdkHandleArgs()));
        MethodHandle basic = invoker(hGetI.type(), "basicInvoker");
        row("basic-invoker-type", () -> basic.type());
        row("basic-invoker", () -> (int) basic.invokeExact(hGetI, (Object) vi, (Object) new L4W54RealJdkHandleArgs()));
        MethodHandle generic = invoker(hGetI.type(), "genericInvoker");
        row("generic-invoker", () -> (int) generic.invokeExact(hGetI, vi, new L4W54RealJdkHandleArgs()));
        // A real `DirectMethodHandle` (the JDK's own `DirectMethodHandle.make`
        // over a `MemberName`), static and virtual, and a static one whose
        // class is not initialized yet (its form is the `invokeStaticInit`
        // one, which must run the initializer first).
        MethodHandle dStatic = realDirect(me.getDeclaredMethod("twice", int.class));
        MethodHandle dVirtual = realDirect(me.getDeclaredMethod("plus", int.class));
        row("direct-static-class", () -> dStatic.getClass().getName() + " " + dStatic.type());
        row("direct-static-exact", () -> (int) dStatic.invokeExact(21));
        row("direct-static-boxed", () -> dStatic.invoke((Object) Integer.valueOf(21)));
        row("direct-static-iwa", () -> dStatic.invokeWithArguments(21));
        row("direct-virtual-exact", () -> (int) dVirtual.invokeExact(new L4W54RealJdkHandleArgs(), 3));
        row("direct-virtual-iwa", () -> dVirtual.invokeWithArguments(new L4W54RealJdkHandleArgs(), (short) 3));
        row("direct-virtual-null", () -> (int) dVirtual.invokeExact((L4W54RealJdkHandleArgs) null, 3));
        MethodHandle dLazy = realDirect(Lazy.class.getDeclaredMethod("twice", int.class));
        row("direct-lazy", () -> (int) dLazy.invokeExact(5));
        row("direct-static-exact-again", () -> (int) dStatic.invokeExact(21));
        // `asType` of a real handle is a NEW handle; the receiver keeps its
        // type (the cached JDK invoker is shared).
        MethodType generic2 = MethodType.methodType(Object.class, VarHandle.class, Object.class);
        row("astype-type", () -> hGetI.asType(generic2).type());
        row("astype-invoke-exact", () -> {
            MethodHandle g = hGetI.asType(generic2);
            Object r = (Object) g.invokeExact(vi, (Object) new L4W54RealJdkHandleArgs());
            return r + " " + r.getClass().getSimpleName();
        });
        row("astype-receiver-type", () -> hGetI.type());
        row("astype-receiver-exact", () -> (int) hGetI.invokeExact(vi, new L4W54RealJdkHandleArgs()));
        row("bindto-type", () -> hGetI.bindTo(vi).type());
        row("bindto-invoke", () -> (int) hGetI.bindTo(vi).invoke(new L4W54RealJdkHandleArgs()));
        row("bindto-receiver-type", () -> hGetI.type());
        row("set-int-boxed-again", () -> {
            L4W54RealJdkHandleArgs o = new L4W54RealJdkHandleArgs();
            hSetI.invoke(vi, o, (Object) Integer.valueOf(7));
            return o.i;
        });
        row("set-long-widened-again", () -> {
            L4W54RealJdkHandleArgs o = new L4W54RealJdkHandleArgs();
            hSetL.invoke(vl, o, 5);
            return o.l;
        });
    }
}
