// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4 (review): `MethodHandle.invokeWithArguments`
// at its arity edges, both overloads.
//
//   list-null     `invokeWithArguments((List) null)`: the JDK body is
//                 `invokeWithArguments(arguments.toArray())`, a helpful NPE
//   array-null    `invokeWithArguments((Object[]) null)`: zero arguments
//   fewer, more   a `(String,String)String` handle given 1 or 3 arguments
//   list-fewer    the same through the `List` overload
//   varargs       `String.format` (a varargs collector) with 3 trailing values
//   varargs-none  the same with no trailing value
//   varargs-300   `Arrays.asList` (varargs) with 300 arguments (jumbo list)
//   fixed-300     a `(Object[])Object` fixed-arity handle given 300 arguments
//
// Run: javac -d out L4W43InvokeWithArgumentsEdges.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W43InvokeWithArgumentsEdges
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible:
//   list-null: java.lang.NullPointerException: Cannot invoke "java.util.List.toArray()" because "arguments" is null
//   array-null: k
//   array-null-cat: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String)String to ()Object
//   fewer: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String)String to (Object)Object
//   more: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String)String to (Object,Object,Object)Object
//   list-fewer: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String)String to (Object)Object
//   list-ok: ab
//   varargs: 1-b-c
//   varargs-none: x
//   varargs-array: java.util.MissingFormatArgumentException: Format specifier '%s'
//   varargs-300: 300
//   fixed-300: java.lang.IllegalArgumentException: bad parameter count 300
//   fixed-1-array: 1:z
//
// Read from the code, CratonVM before wave 43 (every mode): `list-null`
// invoked the handle with no arguments (the `WrongMethodTypeException` of
// `array-null-cat`), and `fixed-300` was the arity's
// `WrongMethodTypeException` (`mh_entry_adapt`). The other rows are
// controls, not traced on the host.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.List;

public class L4W43InvokeWithArgumentsEdges {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof Object[] a ? Arrays.toString(a) : String.valueOf(v);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static String cat(String a, String b) {
        return a + b;
    }

    static String k() {
        return "k";
    }

    static Object first(Object[] a) {
        return a.length + ":" + a[0];
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle cat = l.findStatic(L4W43InvokeWithArgumentsEdges.class, "cat",
                MethodType.methodType(String.class, String.class, String.class));
        MethodHandle zero = l.findStatic(L4W43InvokeWithArgumentsEdges.class, "k",
                MethodType.methodType(String.class));
        MethodHandle format = l.findStatic(String.class, "format",
                MethodType.methodType(String.class, String.class, Object[].class));
        MethodHandle asList = l.findStatic(Arrays.class, "asList", MethodType.methodType(List.class, Object[].class));
        MethodHandle first = l.findStatic(L4W43InvokeWithArgumentsEdges.class, "first",
                MethodType.methodType(Object.class, Object[].class));
        row("list-null", () -> cat.invokeWithArguments((List<?>) null));
        row("array-null", () -> zero.invokeWithArguments((Object[]) null));
        row("array-null-cat", () -> cat.invokeWithArguments((Object[]) null));
        row("fewer", () -> cat.invokeWithArguments("a"));
        row("more", () -> cat.invokeWithArguments("a", "b", "c"));
        row("list-fewer", () -> cat.invokeWithArguments(List.of("a")));
        row("list-ok", () -> cat.invokeWithArguments(List.of("a", "b")));
        row("varargs", () -> format.invokeWithArguments("%s-%s-%s", 1, "b", 'c'));
        row("varargs-none", () -> format.invokeWithArguments("x"));
        row("varargs-array", () -> format.invokeWithArguments("%s/%s", new Object[] {"p", "q"}));
        Object[] many = new Object[300];
        for (int i = 0; i < many.length; i++) {
            many[i] = i;
        }
        row("varargs-300", () -> ((List<?>) asList.invokeWithArguments(many)).size());
        row("fixed-300", () -> first.invokeWithArguments(many));
        row("fixed-1-array", () -> first.invokeWithArguments((Object) new Object[] {"z"}));
    }
}
