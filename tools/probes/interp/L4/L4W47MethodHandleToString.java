// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L4: `MethodHandle.toString()` is
// `"MethodHandle" + type()` (`standardString()`, JDK 25). CratonVM's native
// answered the bare word `MethodHandle` for every handle, so a log line or an
// exception text quoting a handle lost its signature.
//
// Positive control: row `static` (the base printed `MethodHandle`).
//
// Run: javac -d out L4W47MethodHandleToString.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W47MethodHandleToString
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   static: MethodHandle(int)int
//   virtual: MethodHandle(L4W47MethodHandleToString,String)String
//   bound: MethodHandle(String)String
//   ctor: MethodHandle()ArrayList
//   getter: MethodHandle(L4W47MethodHandleToString)int
//   constant: MethodHandle()String
//   identity: MethodHandle(Object)Object
//   array-ctor: MethodHandle(int)int[]
//   as-type: MethodHandle(Integer)Object
//   varargs: MethodHandle(String,Object[])String
//   vh-to-mh: MethodHandle(L4W47MethodHandleToString)int
//   concat: MethodHandle(int)int MethodHandle(int)int
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;

public class L4W47MethodHandleToString {
    int field;

    static int twice(int x) {
        return 2 * x;
    }

    public String inst(String a) {
        return "i" + a;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle tw = l.findStatic(L4W47MethodHandleToString.class, "twice",
                MethodType.methodType(int.class, int.class));
        MethodHandle inst = l.findVirtual(L4W47MethodHandleToString.class, "inst",
                MethodType.methodType(String.class, String.class));
        System.out.println("static: " + tw);
        System.out.println("virtual: " + inst);
        System.out.println("bound: " + inst.bindTo(new L4W47MethodHandleToString()));
        System.out.println("ctor: " + l.findConstructor(ArrayList.class, MethodType.methodType(void.class)));
        System.out.println("getter: " + l.findGetter(L4W47MethodHandleToString.class, "field", int.class));
        System.out.println("constant: " + MethodHandles.constant(String.class, "c"));
        System.out.println("identity: " + MethodHandles.identity(Object.class));
        System.out.println("array-ctor: " + MethodHandles.arrayConstructor(int[].class));
        System.out.println("as-type: " + tw.asType(MethodType.methodType(Object.class, Integer.class)));
        System.out.println("varargs: " + l.findStatic(String.class, "format",
                MethodType.methodType(String.class, String.class, Object[].class)));
        VarHandle vh = l.findVarHandle(L4W47MethodHandleToString.class, "field", int.class);
        System.out.println("vh-to-mh: " + vh.toMethodHandle(VarHandle.AccessMode.GET));
        System.out.println("concat: " + tw + " " + String.valueOf(tw));
    }
}
