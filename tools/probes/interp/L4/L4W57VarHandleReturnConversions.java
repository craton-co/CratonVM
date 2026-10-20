// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L4: `asType`'s RETURN direction of a
// VarHandle access mode that returns its variable, at a PRIMITIVE call site.
// The variable's type must convert (`MethodType.canConvert`) to the site's
// return or HotSpot raises `WrongMethodTypeException` before the access:
// a narrowing primitive (`(int)` over a `long` field), a reference that is
// neither a wrapper supertype nor a wrapper widening to it (`String`,
// `int[]`), and a primitive into a reference site that is not a supertype of
// its wrapper (`(String)` or `(Long)` over an `int` field). A reference that
// converts (`Object`, `Number`, `Integer` into `long`) is unboxed at run time.
// CratonVM answered `0` for the refused primitive-site rows and W6-1's own
// text for the reference-site rows (page
// `interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021`,
// switch `CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE`). The `object-*` rows document
// the run-time unboxing HotSpot makes of a reference variable.
//
// Run: javac -d out L4W57VarHandleReturnConversions.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W57VarHandleReturnConversions
//
// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical):
//   long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)long to (VarHandle,L4W57VarHandleReturnConversions)int
//   static-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle)long to (VarHandle)int
//   array-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,long[],int)long to (VarHandle,long[],int)int
//   int-array-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)int[] to (VarHandle,L4W57VarHandleReturnConversions)int
//   string-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)String to (VarHandle,L4W57VarHandleReturnConversions)int
//   int-as-char: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)int to (VarHandle,L4W57VarHandleReturnConversions)char
//   int-as-boolean: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)int to (VarHandle,L4W57VarHandleReturnConversions)boolean
//   int-as-double: 4.0
//   object-as-int: 5
//   object-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   object-null-as-int: NullPointerException
//   number-as-int: 8
//   integer-as-long: 9
//   integer-as-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)Integer to (VarHandle,L4W57VarHandleReturnConversions)short
//   int-as-string: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)int to (VarHandle,L4W57VarHandleReturnConversions)String
//   int-as-long-wrapper: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)int to (VarHandle,L4W57VarHandleReturnConversions)Long
//   int-as-number: 4
//   int-as-integer: 4
//   long-as-constantdesc: 40
//   object-short-as-int: 3
//   object-char-as-int: 120
//   object-string-as-int: ClassCastException
//   getandset-long-as-int: java.lang.invoke.WrongMethodTypeException field=40
//   getandadd-long-as-int: java.lang.invoke.WrongMethodTypeException field=40
//   cae-long-as-int: java.lang.invoke.WrongMethodTypeException field=40
//   getandbitwiseor-long-as-int: java.lang.invoke.WrongMethodTypeException field=40
//   invoker-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)long to (VarHandle,L4W57VarHandleReturnConversions)int
//   volatile-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)long to (VarHandle,L4W57VarHandleReturnConversions)int
//   long-as-int-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnConversions)long to (VarHandle,L4W57VarHandleReturnConversions)int
//   long-as-long-control: 40
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W57VarHandleReturnConversions {
    int i = 4;
    long l = 40;
    String str = "x";
    Object obj = 5;
    Object objLong = 6L;
    Object objNull = null;
    Object objShort = (short) 3;
    Object objChar = 'x';
    Object objString = "y";
    Number num = 8;
    Integer boxed = 9;
    int[] arr = {1};
    static long sl = 50;

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

    static L4W57VarHandleReturnConversions o() {
        return new L4W57VarHandleReturnConversions();
    }

    public static void main(String[] args) throws Throwable {
        Class<?> me = L4W57VarHandleReturnConversions.class;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle vi = lk.findVarHandle(me, "i", int.class);
        VarHandle vl = lk.findVarHandle(me, "l", long.class);
        VarHandle vstr = lk.findVarHandle(me, "str", String.class);
        VarHandle vobj = lk.findVarHandle(me, "obj", Object.class);
        VarHandle vobjLong = lk.findVarHandle(me, "objLong", Object.class);
        VarHandle vobjNull = lk.findVarHandle(me, "objNull", Object.class);
        VarHandle vobjShort = lk.findVarHandle(me, "objShort", Object.class);
        VarHandle vobjChar = lk.findVarHandle(me, "objChar", Object.class);
        VarHandle vobjString = lk.findVarHandle(me, "objString", Object.class);
        VarHandle vnum = lk.findVarHandle(me, "num", Number.class);
        VarHandle vboxed = lk.findVarHandle(me, "boxed", Integer.class);
        VarHandle varr = lk.findVarHandle(me, "arr", int[].class);
        VarHandle vsl = lk.findStaticVarHandle(me, "sl", long.class);
        VarHandle va = MethodHandles.arrayElementVarHandle(long[].class);
        long[] longs = {70};
        MethodHandle getInt = MethodHandles.varHandleInvoker(VarHandle.AccessMode.GET, MethodType.methodType(int.class, me));
        row("long-as-int", () -> {
            int x = (int) vl.get(o());
            return x;
        });
        row("static-long-as-int", () -> {
            int x = (int) vsl.get();
            return x;
        });
        row("array-long-as-int", () -> {
            int x = (int) va.get(longs, 0);
            return x;
        });
        row("int-array-as-int", () -> {
            int x = (int) varr.get(o());
            return x;
        });
        row("string-as-int", () -> {
            int x = (int) vstr.get(o());
            return x;
        });
        row("int-as-char", () -> {
            char x = (char) vi.get(o());
            return (int) x;
        });
        row("int-as-boolean", () -> {
            boolean x = (boolean) vi.get(o());
            return x;
        });
        row("int-as-double", () -> {
            double x = (double) vi.get(o());
            return x;
        });
        row("object-as-int", () -> {
            int x = (int) vobj.get(o());
            return x;
        });
        row("object-long-as-int", () -> {
            int x = (int) vobjLong.get(o());
            return x;
        });
        row("object-null-as-int", () -> {
            // The class only: HotSpot's helpful-NPE text names JDK internals.
            try {
                int x = (int) vobjNull.get(o());
                return x;
            } catch (NullPointerException e) {
                return "NullPointerException";
            }
        });
        row("number-as-int", () -> {
            int x = (int) vnum.get(o());
            return x;
        });
        row("integer-as-long", () -> {
            long x = (long) vboxed.get(o());
            return x;
        });
        row("integer-as-short", () -> {
            short x = (short) vboxed.get(o());
            return x;
        });
        row("int-as-string", () -> {
            String x = (String) vi.get(o());
            return x;
        });
        row("int-as-long-wrapper", () -> {
            Long x = (Long) vi.get(o());
            return x;
        });
        row("int-as-number", () -> {
            Number x = (Number) vi.get(o());
            return x;
        });
        row("int-as-integer", () -> {
            Integer x = (Integer) vi.get(o());
            return x;
        });
        row("long-as-constantdesc", () -> {
            java.lang.constant.ConstantDesc x = (java.lang.constant.ConstantDesc) vl.get(o());
            return x;
        });
        row("object-short-as-int", () -> {
            int x = (int) vobjShort.get(o());
            return x;
        });
        row("object-char-as-int", () -> {
            int x = (int) vobjChar.get(o());
            return x;
        });
        row("object-string-as-int", () -> {
            // The class only: HotSpot's text names the modules and loaders.
            try {
                int x = (int) vobjString.get(o());
                return x;
            } catch (ClassCastException e) {
                return "ClassCastException";
            }
        });
        row("getandset-long-as-int", () -> {
            L4W57VarHandleReturnConversions t = o();
            String r;
            try {
                int x = (int) vl.getAndSet(t, 3L);
                r = String.valueOf(x);
            } catch (Throwable e) {
                r = e.getClass().getName();
            }
            return r + " field=" + t.l;
        });
        row("getandadd-long-as-int", () -> {
            L4W57VarHandleReturnConversions t = o();
            String r;
            try {
                int x = (int) vl.getAndAdd(t, 1L);
                r = String.valueOf(x);
            } catch (Throwable e) {
                r = e.getClass().getName();
            }
            return r + " field=" + t.l;
        });
        row("cae-long-as-int", () -> {
            L4W57VarHandleReturnConversions t = o();
            String r;
            try {
                int x = (int) vl.compareAndExchange(t, 40L, 41L);
                r = String.valueOf(x);
            } catch (Throwable e) {
                r = e.getClass().getName();
            }
            return r + " field=" + t.l;
        });
        row("getandbitwiseor-long-as-int", () -> {
            L4W57VarHandleReturnConversions t = o();
            String r;
            try {
                int x = (int) vl.getAndBitwiseOr(t, 1L);
                r = String.valueOf(x);
            } catch (Throwable e) {
                r = e.getClass().getName();
            }
            return r + " field=" + t.l;
        });
        row("invoker-long-as-int", () -> (int) getInt.invoke(vl, o()));
        row("volatile-long-as-int", () -> {
            int x = (int) vl.getVolatile(o());
            return x;
        });
        row("long-as-int-again", () -> {
            int x = (int) vl.get(o());
            return x;
        });
        row("long-as-long-control", () -> {
            long x = (long) vl.get(o());
            return x;
        });
    }
}
