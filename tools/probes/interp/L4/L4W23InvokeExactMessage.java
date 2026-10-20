// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L4: the WrongMethodTypeException
// message of a `MethodHandle.invokeExact` whose call-site type is not the
// handle's type. JDK 25 words it `handle's method type <T> but found <S>`
// (`Invokers.newWrongMethodTypeException`); older JDKs said `expected <T>
// but found <S>`, which is what CratonVM's `exact_call_site_refusal`
// (native-builtins/src/lang_invoke.rs) printed before wave 23.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W23InvokeExactMessage
//
// HotSpot 25 (25.0.3) prints:
//   long args: java.lang.invoke.WrongMethodTypeException: handle's method type (int,int)int but found (long,long)int
//   boxed args: java.lang.invoke.WrongMethodTypeException: handle's method type (int,int)int but found (Integer,Integer)int
//   Object return: java.lang.invoke.WrongMethodTypeException: handle's method type (int,int)int but found (int,int)Object
//   one arg: java.lang.invoke.WrongMethodTypeException: handle's method type (int,int)int but found (int)int
//   exact: 7
//   receiver widened: java.lang.invoke.WrongMethodTypeException: handle's method type (String)int but found (Object)int
//   invoke converts: 4

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W23InvokeExactMessage {
    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            out = String.valueOf(c.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle max = MethodHandles.lookup().findStatic(Math.class, "max",
                MethodType.methodType(int.class, int.class, int.class));
        MethodHandle len = MethodHandles.lookup().findVirtual(String.class, "length",
                MethodType.methodType(int.class));
        row("long args", () -> (int) max.invokeExact(1L, 2L));
        row("boxed args", () -> (int) max.invokeExact(Integer.valueOf(1), Integer.valueOf(2)));
        row("Object return", () -> (Object) max.invokeExact(1, 2));
        row("one arg", () -> (int) max.invokeExact(1));
        row("exact", () -> (int) max.invokeExact(7, 3));
        row("receiver widened", () -> (int) len.invokeExact((Object) "abcd"));
        row("invoke converts", () -> (int) len.invoke((Object) "abcd"));
    }
}
