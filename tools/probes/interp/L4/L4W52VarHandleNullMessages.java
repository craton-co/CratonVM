// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L4 (review): the MESSAGES of the
// exceptions a VarHandle access raises for a null coordinate and for a
// value of the wrong type. The exception classes match HotSpot since
// 2026-09-02 (`regression-suite/src/RJdkVarHandleNullCoord`); the texts do
// not. Page
// `docs/internal/fixed-bugs/interpreter-L4-varhandle-null-coordinate-and-value-type-messages-differ-FIXED-20261017.md`.
//
// Run: javac -d out L4W52VarHandleNullMessages.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W52VarHandleNullMessages
//
// Expected HotSpot 25 output (25.0.3, default flags):
//   array-get: java.lang.NullPointerException: Cannot load from int array because "array" is null
//   array-set: java.lang.NullPointerException: Cannot store to int array because "array" is null
//   array-cas: java.lang.NullPointerException: Cannot read the array length because "array" is null
//   array-getAndAdd: java.lang.NullPointerException: Cannot read the array length because "array" is null
//   ref-array-get: java.lang.NullPointerException: Cannot load from object array because "array" is null
//   instance-get: java.lang.NullPointerException
//   instance-set: java.lang.NullPointerException
//   array-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,int[],int,int)int to (VarHandle,int[],int,double)Object
//   instance-wrong-value: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,H,int)void to (VarHandle,H,String)void
//   array-get-again: java.lang.NullPointerException: Cannot load from int array because "array" is null
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W52VarHandleNullMessages {
    public static class H {
        public int i;
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
        VarHandle ia = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle oa = MethodHandles.arrayElementVarHandle(Object[].class);
        VarHandle hi = MethodHandles.lookup().findVarHandle(H.class, "i", int.class);
        row("array-get", () -> (int) ia.get((int[]) null, 0));
        row("array-set", () -> {
            ia.set((int[]) null, 0, 1);
            return "void";
        });
        row("array-cas", () -> ia.compareAndSet((int[]) null, 0, 0, 1));
        row("array-getAndAdd", () -> (int) ia.getAndAdd((int[]) null, 0, 1));
        row("ref-array-get", () -> oa.get((Object[]) null, 0));
        row("instance-get", () -> (int) hi.get((H) null));
        row("instance-set", () -> {
            hi.set((H) null, 1);
            return "void";
        });
        row("array-wrong-value", () -> ia.getAndAdd(new int[1], 0, 1.0));
        row("instance-wrong-value", () -> {
            hi.set(new H(), "x");
            return "void";
        });
        row("array-get-again", () -> (int) ia.get((int[]) null, 0));
    }
}
