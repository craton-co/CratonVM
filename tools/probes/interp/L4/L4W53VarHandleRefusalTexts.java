// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L4: the MESSAGES of the exceptions a
// VarHandle access raises (1) when the call site's static type cannot be
// converted to the access mode type (HotSpot's invoker `asType` refusal,
// which names both method types) and (2) for a null byte-array / ByteBuffer
// view coordinate. Page
// `docs/internal/fixed-bugs/interpreter-L4-varhandle-null-coordinate-and-value-type-messages-differ-FIXED-20261017.md`.
//
// Run: javac -d out L4W53VarHandleRefusalTexts.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W53VarHandleRefusalTexts
//
// Expected HotSpot 25 output (25.0.3, default flags):
//   array-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,int[],int,int)int to (VarHandle,int[],int,double)Object
//   view-get-null: java.lang.NullPointerException: Cannot read the array length because "ba" is null
//   instance-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,H,int)void to (VarHandle,H,String)void
//   instance-cas-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,H,int,int)boolean to (VarHandle,H,int,String)boolean
//   static-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,int)void to (VarHandle,String)void
//   array-long-index: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,int[],int)int to (VarHandle,int[],long)int
//   reference-primitive-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,H,String)void to (VarHandle,H,int)void
//   view-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,byte[],int,int)void to (VarHandle,byte[],int,long)void
//   instance-arity: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,H)int to (VarHandle)int
//   array-arity: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,int[],int)int to (VarHandle,int[])int
//   array-set-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,int[],int,int)void to (VarHandle,int[],int,long)void
//   view-set-null: java.lang.NullPointerException: Cannot read the array length because "ba" is null
//   view-getVolatile-null: java.lang.UnsupportedOperationException: getVolatile
//   view-cas-null: java.lang.UnsupportedOperationException: compareAndSet
//   view-getAndAdd-null: java.lang.UnsupportedOperationException: getAndAdd
//   buffer-get-null: java.lang.NullPointerException
//   buffer-set-null: java.lang.NullPointerException
//   buffer-cas-null: java.lang.NullPointerException
//   buffer-getAndAdd-null: java.lang.NullPointerException
//   array-wrong-value-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,int[],int,int)int to (VarHandle,int[],int,double)Object
//   view-get-null-again: java.lang.NullPointerException: Cannot read the array length because "ba" is null
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class L4W53VarHandleRefusalTexts {
    public static class H {
        public int i;
        public String s;
        public static int st;
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body b) {
        String out;
        try {
            out = "returned " + b.run();
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle ia = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle hi = l.findVarHandle(H.class, "i", int.class);
        VarHandle hs = l.findVarHandle(H.class, "s", String.class);
        VarHandle st = l.findStaticVarHandle(H.class, "st", int.class);
        VarHandle bv = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
        VarHandle bb = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);

        // The fixed cases first (cold VM), and again at the end.
        row("array-wrong-value", () -> ia.getAndAdd(new int[1], 0, 1.0));
        row("view-get-null", () -> (int) bv.get((byte[]) null, 0));

        row("instance-wrong-value", () -> {
            hi.set(new H(), "x");
            return "void";
        });
        row("instance-cas-wrong-value", () -> hi.compareAndSet(new H(), 0, "x"));
        row("static-wrong-value", () -> {
            st.set("x");
            return "void";
        });
        row("array-long-index", () -> (int) ia.get(new int[1], 0L));
        row("reference-primitive-value", () -> {
            hs.set(new H(), 5);
            return "void";
        });
        row("view-wrong-value", () -> {
            bv.set(new byte[8], 0, 5L);
            return "void";
        });
        row("instance-arity", () -> (int) hi.get());
        row("array-arity", () -> (int) ia.get(new int[1]));
        row("array-set-wrong-value", () -> {
            ia.set(new int[1], 0, 1L);
            return "void";
        });

        row("view-set-null", () -> {
            bv.set((byte[]) null, 0, 1);
            return "void";
        });
        row("view-getVolatile-null", () -> (int) bv.getVolatile((byte[]) null, 0));
        row("view-cas-null", () -> bv.compareAndSet((byte[]) null, 0, 0, 1));
        row("view-getAndAdd-null", () -> (int) bv.getAndAdd((byte[]) null, 0, 1));
        row("buffer-get-null", () -> (int) bb.get((ByteBuffer) null, 0));
        row("buffer-set-null", () -> {
            bb.set((ByteBuffer) null, 0, 1);
            return "void";
        });
        row("buffer-cas-null", () -> bb.compareAndSet((ByteBuffer) null, 0, 0, 1));
        row("buffer-getAndAdd-null", () -> (int) bb.getAndAdd((ByteBuffer) null, 0, 1));

        row("array-wrong-value-again", () -> ia.getAndAdd(new int[1], 0, 1.0));
        row("view-get-null-again", () -> (int) bv.get((byte[]) null, 0));
    }
}
