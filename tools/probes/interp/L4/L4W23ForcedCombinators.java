// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L4: the `MethodHandles` / `MethodHandle`
// combinators CratonVM answers with its own natives even under --jdk-only
// (`vm_exec.rs`'s forced-native allow-list: `constant`, `identity`, `zero`,
// `empty`, `insertArguments`, `arrayElementGetter` / `Setter`,
// `arrayLength`, `countedLoop`, `tryFinally`, `asCollector`, `asSpreader`,
// `asVarargsCollector`, `asFixedArity`), against HotSpot: the handle's type,
// its result, and the exception (class and message) for a bad argument.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W23ForcedCombinators
//
// HotSpot 25 (25.0.3) prints:
//   constant long from Integer: 5
//   constant int from Long: java.lang.ClassCastException: class java.lang.Integer is not compatible with class java.lang.Long
//   constant String from Integer: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   constant void: java.lang.IllegalArgumentException: void type
//   constant int null: java.lang.NullPointerException: Cannot invoke "Object.getClass()" because "x" is null
//   constant Object null: null
//   constant type: handle ()Number
//   identity void: java.lang.IllegalArgumentException: parameter type cannot be void
//   identity int: 9
//   zero double: 0.0
//   zero void: handle ()void
//   zero String: null
//   empty type: handle (String,int)boolean
//   empty result: false
//   insert type: handle (String,String)String
//   insert result: ABC
//   insert two: XYZ
//   insert pos too big: java.lang.IllegalArgumentException: no argument type to append
//   insert too many: java.lang.IllegalArgumentException: no argument type to append
//   insert wrong type: java.lang.ClassCastException: class java.lang.Integer is not compatible with class java.lang.String
//   insert null into int: java.lang.NullPointerException: Cannot invoke "Object.getClass()" because "x" is null
//   insert unbox: 42
//   arrayElementGetter not array: java.lang.IllegalArgumentException: not an array: class java.lang.String
//   arrayElementGetter type: handle (int[],int)int
//   arrayElementGetter oob: java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
//   arrayElementSetter store: java.lang.ArrayStoreException: java.lang.Integer
//   arrayElementGetter null int[]: java.lang.NullPointerException: Cannot load from int array because "a" is null
//   arrayElementGetter null boolean[]: java.lang.NullPointerException: Cannot load from byte/boolean array because "a" is null
//   arrayElementGetter null Object[]: java.lang.NullPointerException: Cannot load from object array because "a" is null
//   arrayElementSetter null long[]: java.lang.NullPointerException: Cannot store to long array because "a" is null
//   arrayLength not array: java.lang.IllegalArgumentException: not an array: int
//   arrayLength null: java.lang.NullPointerException: Cannot read the array length because "a" is null
//   countedLoop: 10
//   tryFinally rethrows: java.lang.IllegalStateException: boom 3
//   tryFinally type: handle (int)int
//   tryFinally bad cleanup: java.lang.IllegalArgumentException: cleanup first argument and Throwable must match: (int,int)int != class java.lang.Throwable
//   asCollector type: handle (String,String)String
//   asCollector result: a|b
//   asCollector not array: java.lang.IllegalArgumentException: not an array type: class java.lang.String
//   asCollector wrong array: java.lang.IllegalArgumentException: array type not assignable to argument: MethodHandle(String[])String, class [Ljava.lang.Integer;
//   asSpreader type: handle (String,String[])String
//   asSpreader result: ABC
//   asSpreader short array: java.lang.IllegalArgumentException: array is not of length 2
//   asSpreader null array: java.lang.NullPointerException: null array reference
//   asSpreader too many: java.lang.IllegalArgumentException: bad spread array length
//   asVarargsCollector not array: java.lang.IllegalArgumentException: not an array type: int
//   asVarargsCollector invoke: p|q|r
//   asVarargsCollector no params: java.lang.IllegalArgumentException: bad collect position
//   asCollector illegal length: java.lang.IllegalArgumentException: array length is not legal: 300
//   asSpreader not array: java.lang.IllegalArgumentException: not an array type: class java.lang.String
//   int[] collector: 3
//
// Wave 24 (lane L4): the wave-23 Linux run (default mode) still differed in
// two rows, `arrayElementGetter oob` (`java.lang.ArrayIndexOutOfBoundsException:
// null`) and `arrayElementSetter store` (`[1]`: an `Integer` STORED into a
// `String[]`); both are fixed in `mh_dispatch`'s `MH_KIND_ARRAY_GET | _SET`
// arm (`RuntimeError::aioobe`, `array_store::reject_unstorable`). The
// `--compatible` rows that differed on waves 22 and 23 were not reported
// row by row; re-run in both modes.
//
// Before wave 23, read from the code (not run): the refusals of `constant`
// (void, a wrong-typed value), `identity(void.class)`, `arrayLength(int.class)`,
// `insertArguments`' value checks, and every `asCollector` / `asSpreader` /
// `asVarargsCollector` refusal were missing (a handle came back), and
// `constant(int.class, null)`, `arrayElementGetter(String.class)` and the two
// `insertArguments` count refusals carried CratonVM's own messages, as did
// the null-array and wrong-length refusals of the accessor and spreader
// handles at invocation (`<name>: array is null`, `array length does not
// match count: 1 != 2`). The
// invocation rows (`countedLoop`, `tryFinally`, the collector / spreader
// results) exercise the `MH_KIND_*` dispatch and are here as controls.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;

public class L4W23ForcedCombinators {
    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            Object r = c.run();
            if (r instanceof MethodHandle mh) out = "handle " + mh.type();
            else if (r instanceof Object[] a) out = Arrays.toString(a);
            else if (r instanceof int[] a) out = Arrays.toString(a);
            else out = String.valueOf(r);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static int add(int a, int b) { return a + b; }
    static String join(String a, String b, String c) { return a + b + c; }
    static String cat(String... xs) { return String.join("|", xs); }
    static int len(int[] xs) { return xs.length; }
    static int thrower(int x) { throw new IllegalStateException("boom " + x); }
    static int cleanup(Throwable t, int r, int x) { return t == null ? r + 100 : -1; }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle add = l.findStatic(L4W23ForcedCombinators.class, "add",
                MethodType.methodType(int.class, int.class, int.class));
        MethodHandle join = l.findStatic(L4W23ForcedCombinators.class, "join",
                MethodType.methodType(String.class, String.class, String.class, String.class));
        MethodHandle cat = l.findStatic(L4W23ForcedCombinators.class, "cat",
                MethodType.methodType(String.class, String[].class));
        MethodHandle len = l.findStatic(L4W23ForcedCombinators.class, "len",
                MethodType.methodType(int.class, int[].class));

        // constant
        row("constant long from Integer", () -> (long) MethodHandles.constant(long.class, 5).invokeExact());
        row("constant int from Long", () -> MethodHandles.constant(int.class, 5L));
        row("constant String from Integer", () -> MethodHandles.constant(String.class, 5));
        row("constant void", () -> MethodHandles.constant(void.class, null));
        row("constant int null", () -> MethodHandles.constant(int.class, null));
        row("constant Object null", () -> MethodHandles.constant(Object.class, null).invoke());
        row("constant type", () -> MethodHandles.constant(Number.class, 7));
        // identity / zero / empty
        row("identity void", () -> MethodHandles.identity(void.class));
        row("identity int", () -> (int) MethodHandles.identity(int.class).invokeExact(9));
        row("zero double", () -> (double) MethodHandles.zero(double.class).invokeExact());
        row("zero void", () -> MethodHandles.zero(void.class));
        row("zero String", () -> MethodHandles.zero(String.class).invoke());
        row("empty type", () -> MethodHandles.empty(MethodType.methodType(boolean.class, String.class, int.class)));
        row("empty result", () -> (boolean) MethodHandles.empty(MethodType.methodType(boolean.class, String.class)).invokeExact("x"));
        // insertArguments
        row("insert type", () -> MethodHandles.insertArguments(join, 1, "B"));
        row("insert result", () -> (String) MethodHandles.insertArguments(join, 1, "B").invokeExact("A", "C"));
        row("insert two", () -> (String) MethodHandles.insertArguments(join, 0, "X", "Y").invokeExact("Z"));
        row("insert pos too big", () -> MethodHandles.insertArguments(join, 3, "B"));
        row("insert too many", () -> MethodHandles.insertArguments(join, 2, "B", "C"));
        row("insert wrong type", () -> MethodHandles.insertArguments(add, 0, "x"));
        row("insert null into int", () -> MethodHandles.insertArguments(add, 0, (Object) null));
        row("insert unbox", () -> (int) MethodHandles.insertArguments(add, 0, 40).invokeExact(2));
        // array accessors
        row("arrayElementGetter not array", () -> MethodHandles.arrayElementGetter(String.class));
        row("arrayElementGetter type", () -> MethodHandles.arrayElementGetter(int[].class));
        row("arrayElementGetter oob", () -> (int) MethodHandles.arrayElementGetter(int[].class).invokeExact(new int[2], 2));
        row("arrayElementSetter store", () -> {
            Object[] a = new String[1];
            MethodHandles.arrayElementSetter(Object[].class).invokeExact(a, 0, (Object) Integer.valueOf(1));
            return a;
        });
        row("arrayElementGetter null int[]", () -> (int) MethodHandles.arrayElementGetter(int[].class).invokeExact((int[]) null, 0));
        row("arrayElementGetter null boolean[]", () -> (boolean) MethodHandles.arrayElementGetter(boolean[].class).invokeExact((boolean[]) null, 0));
        row("arrayElementGetter null Object[]", () -> (Object) MethodHandles.arrayElementGetter(Object[].class).invokeExact((Object[]) null, 0));
        row("arrayElementSetter null long[]", () -> {
            MethodHandles.arrayElementSetter(long[].class).invokeExact((long[]) null, 0, 1L);
            return "stored";
        });
        row("arrayLength not array", () -> MethodHandles.arrayLength(int.class));
        row("arrayLength null", () -> (int) MethodHandles.arrayLength(int[].class).invokeExact((int[]) null));
        // countedLoop / tryFinally
        MethodHandle body = l.findStatic(Integer.class, "sum", MethodType.methodType(int.class, int.class, int.class));
        row("countedLoop", () -> (int) MethodHandles.countedLoop(MethodHandles.constant(int.class, 5),
                MethodHandles.constant(int.class, 0), body).invokeExact());
        MethodHandle thrower = l.findStatic(L4W23ForcedCombinators.class, "thrower",
                MethodType.methodType(int.class, int.class));
        MethodHandle cleanup = l.findStatic(L4W23ForcedCombinators.class, "cleanup",
                MethodType.methodType(int.class, Throwable.class, int.class, int.class));
        row("tryFinally rethrows", () -> (int) MethodHandles.tryFinally(thrower, cleanup).invokeExact(3));
        row("tryFinally type", () -> MethodHandles.tryFinally(thrower, cleanup));
        row("tryFinally bad cleanup", () -> MethodHandles.tryFinally(thrower, add));
        // asCollector / asSpreader / asVarargsCollector
        row("asCollector type", () -> cat.asCollector(String[].class, 2));
        row("asCollector result", () -> (String) cat.asCollector(String[].class, 2).invokeExact("a", "b"));
        row("asCollector not array", () -> add.asCollector(String.class, 2));
        row("asCollector wrong array", () -> cat.asCollector(Integer[].class, 2));
        row("asSpreader type", () -> join.asSpreader(String[].class, 2));
        row("asSpreader result", () -> (String) join.asSpreader(String[].class, 2).invokeExact("A", new String[] {"B", "C"}));
        row("asSpreader short array", () -> (String) join.asSpreader(String[].class, 2).invokeExact("A", new String[] {"B"}));
        row("asSpreader null array", () -> (String) join.asSpreader(String[].class, 2).invokeExact("A", (String[]) null));
        row("asSpreader too many", () -> join.asSpreader(String[].class, 4));
        row("asVarargsCollector not array", () -> add.asVarargsCollector(int.class));
        row("asVarargsCollector invoke", () -> (String) cat.asVarargsCollector(String[].class).invoke("p", "q", "r"));
        row("asVarargsCollector no params", () -> MethodHandles.constant(int.class, 1).asVarargsCollector(int[].class));
        row("asCollector illegal length", () -> cat.asCollector(String[].class, 300));
        row("asSpreader not array", () -> join.asSpreader(String.class, 1));
        row("int[] collector", () -> (int) len.asCollector(int[].class, 3).invokeExact(1, 2, 3));
    }
}
