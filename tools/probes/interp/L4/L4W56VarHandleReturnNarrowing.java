// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L4 (review): a VarHandle access whose
// call site declares a PRIMITIVE return the variable's type does not convert
// to (`(int) vh.get(o)` over a `long` or a `String` field, `(short)` over an
// `int`) is `asType`'s refusal on HotSpot, a `WrongMethodTypeException`
// naming the access-mode type and the call site. CratonVM (wave 56, local
// debug build, both modes) answers `0`, silently. The parameter direction
// (`set` of an `int` into a `short` field) and the widening rows already
// match. Page
// `docs/internal/fixed-bugs/interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021.md`
// (fixed in wave 57: `CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE`).
//
// Run: javac -d out L4W56VarHandleReturnNarrowing.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W56VarHandleReturnNarrowing
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   direct-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing)long to (VarHandle,L4W56VarHandleReturnNarrowing)int
//   direct-string-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing)String to (VarHandle,L4W56VarHandleReturnNarrowing)int
//   direct-int-as-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing)int to (VarHandle,L4W56VarHandleReturnNarrowing)short
//   direct-int-as-long: 4
//   direct-short-as-int: 7
//   invoker-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing)long to (VarHandle,L4W56VarHandleReturnNarrowing)int
//   invoker-string-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing)String to (VarHandle,L4W56VarHandleReturnNarrowing)int
//   invoker-string-as-object: x
//   set-int-into-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing,short)void to (VarHandle,L4W56VarHandleReturnNarrowing,int)void
//   gaa-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing,long)long to (VarHandle,L4W56VarHandleReturnNarrowing,int)int
//   direct-long-as-int-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W56VarHandleReturnNarrowing)long to (VarHandle,L4W56VarHandleReturnNarrowing)int
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W56VarHandleReturnNarrowing {
    int i = 4;
    long l = 40;
    short s = 7;
    String str = "x";

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

    public static void main(String[] args) throws Throwable {
        Class<?> me = L4W56VarHandleReturnNarrowing.class;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle vi = lk.findVarHandle(me, "i", int.class);
        VarHandle vl = lk.findVarHandle(me, "l", long.class);
        VarHandle vs = lk.findVarHandle(me, "s", short.class);
        VarHandle vstr = lk.findVarHandle(me, "str", String.class);
        MethodHandle getInt = MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, MethodType.methodType(int.class, me));
        MethodHandle getObj = MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, MethodType.methodType(Object.class, me));
        MethodHandle setInt = MethodHandles.varHandleInvoker(VarHandle.AccessMode.SET, MethodType.methodType(void.class, me, int.class));
        row("direct-long-as-int", () -> {
            int x = (int) vl.get(new L4W56VarHandleReturnNarrowing());
            return x;
        });
        row("direct-string-as-int", () -> {
            int x = (int) vstr.get(new L4W56VarHandleReturnNarrowing());
            return x;
        });
        row("direct-int-as-short", () -> {
            short x = (short) vi.get(new L4W56VarHandleReturnNarrowing());
            return x;
        });
        row("direct-int-as-long", () -> {
            long x = (long) vi.get(new L4W56VarHandleReturnNarrowing());
            return x;
        });
        row("direct-short-as-int", () -> {
            int x = (int) vs.get(new L4W56VarHandleReturnNarrowing());
            return x;
        });
        row("invoker-long-as-int", () -> (int) getInt.invoke(vl, new L4W56VarHandleReturnNarrowing()));
        row("invoker-string-as-int", () -> (int) getInt.invoke(vstr, new L4W56VarHandleReturnNarrowing()));
        row("invoker-string-as-object", () -> getObj.invoke(vstr, new L4W56VarHandleReturnNarrowing()));
        row("set-int-into-short", () -> {
            setInt.invoke(vs, new L4W56VarHandleReturnNarrowing(), 3);
            return "no-throw";
        });
        row("gaa-long-as-int", () -> {
            int x = (int) vl.getAndAdd(new L4W56VarHandleReturnNarrowing(), 1);
            return x;
        });
        row("direct-long-as-int-again", () -> {
            int x = (int) vl.get(new L4W56VarHandleReturnNarrowing());
            return x;
        });
    }
}
