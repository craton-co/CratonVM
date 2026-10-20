// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L4 (bug hunt): the `MethodHandles`
// factories and `MethodHandle` / `VarHandle` adapters the wave-44 brief names,
// at their edges, one line per case.
//
// Run: javac -d out L4W44CombinatorHunt.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W44CombinatorHunt
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   arrayConstructor-int: (int)int[] 3
//   arrayConstructor-string2d: (int)String[][] 2
//   arrayConstructor-negative: java.lang.NegativeArraySizeException: -1
//   arrayConstructor-notarray: java.lang.IllegalArgumentException: not an array class: java.lang.String
//   arrayConstructor-null: java.lang.NullPointerException: Cannot invoke "java.lang.Class.isArray()" because "arrayClass" is null
//   arrayLength-long: (long[])int 7
//   arrayLength-object: (Object[])int 2
//   arrayLength-null-array: java.lang.NullPointerException: Cannot read the array length because "a" is null
//   arrayLength-notarray: java.lang.IllegalArgumentException: not an array: int
//   arrayElementVarHandle-int: int [class [I, int] [1, 20, 8] 3
//   arrayElementVarHandle-cas-string: true false [z, y]
//   arrayElementVarHandle-oob: java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
//   arrayElementVarHandle-store-check: java.lang.ArrayStoreException: java.lang.Integer
//   arrayElementVarHandle-notarray: java.lang.IllegalArgumentException: not an array: class java.lang.String
//   tableSwitch: (int)String A0 B1 D2 D-1
//   tableSwitch-no-cases: java.lang.IllegalArgumentException: Not enough cases: []
//   tableSwitch-type-mismatch: java.lang.IllegalArgumentException: Case actions must have the same type: [MethodHandle(long)String]
//   tableSwitch-bad-leading: java.lang.IllegalArgumentException: Not enough cases: []
//   tableSwitch-null-case: java.lang.NullPointerException: null
//   countedLoop: (int)int 10
//   countedLoop-range: ()int 118
//   countedLoop-void: ()void [0, 1, 2]
//   iteratedLoop: (Iterable)String abc
//   iteratedLoop-iterator: (List)String nullxy
//   whileLoop: ()int 64
//   doWhileLoop: 6
//   whileLoop-bad-pred: java.lang.IllegalArgumentException: loop predicate must match: (int)int != (int)boolean
//   whileLoop-short-clauses: (int)int 22
//   countedLoop-bad-iterations: java.lang.IllegalArgumentException: start function must match: ()long != ()int
//   countedLoop-bad-body: java.lang.IllegalArgumentException: body function must match: (int)int != (int,int)int
//   countedLoop-null-iterations: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.type()" because "iterations" is null
//   countedLoop-outer-from-end: (int)int 6
//   zero-int: ()int 0
//   zero-object: ()String null
//   zero-void: ()void
//   empty: (String,int)double 0.0
//   constant-widen: ()long 7
//   constant-bad: java.lang.ClassCastException: class java.lang.Integer is not compatible with class java.lang.String
//   constant-void: java.lang.IllegalArgumentException: void type
//   constant-null-prim: java.lang.NullPointerException: Cannot invoke "Object.getClass()" because "x" is null
//   withVarargs: true 6 0
//   withVarargs-false: false (int[])int
//   withVarargs-notarray: java.lang.IllegalArgumentException: not an array type: int
//   asCollector-0: ()int 0
//   asCollector-3: (int,int,int)int 6
//   asCollector-pos: (String,Object,Object)String a-b
//   asCollector-neg: java.lang.IllegalArgumentException: array length is not legal: -1
//   asCollector-wrongtype: java.lang.IllegalArgumentException: array type not assignable to argument: MethodHandle(int[])int, class [J
//   asCollector-toomany: java.lang.IllegalArgumentException: array length is not legal: 256
//   asSpreader-0: (Object[])String k
//   asSpreader-2: (String[])String ab
//   asSpreader-wronglen: java.lang.IllegalArgumentException: array is not of length 2
//   asSpreader-pos: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String)String to (Object)String
//   asSpreader-toomany: java.lang.IllegalArgumentException: bad spread array length
//   asSpreader-neg: java.lang.IllegalArgumentException: array length is not legal: -1
//   vh-exact-behavior: false true 5 false
//   vh-exact-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()long
//   vh-exact-set-boxed: java.lang.invoke.WrongMethodTypeException: handle's method type (int)void but found (Integer)void
//   vh-exact-array: 4 true
//   vh-exact-array-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int)int but found (Object,int)int
//
// Read from the code, the base `5248262b7` (every mode): `tableSwitch*` ran
// the JDK's `MethodHandleImpl.makeTableSwitch` (a `BoundMethodHandle`
// species over a spun `LambdaForm`, which the `MH_KIND_*` model cannot run;
// wave 44 adds the `MH_KIND_TABLE_SWITCH` native); `countedLoop-range` (the
// four-handle `countedLoop`) had no native and ran the JDK's generic `loop`;
// `iteratedLoop` typed its loop `(Iterable,Iterable)String` and handed its
// body the arguments after the iterable; `iteratedLoop-iterator`'s
// non-null `iterator` handle was dropped (the loop called `iterator()` on its
// first argument, here a `List`, by luck the same answer: the row checks the
// loop's type, `(Iterable)String` on the base); `whileLoop-bad-pred` and the
// `countedLoop-bad-*` rows were accepted (no `whileLoopChecks` /
// `countedLoopChecks`), `countedLoop-null-iterations` answered a null handle;
// `countedLoop-outer-from-end` typed its loop `()int` (the body's), where the
// JDK takes the iterations handle's `(int)`; `whileLoop-short-clauses` handed
// every clause all the arguments. The other rows are the brief's edge list;
// the host run names any further difference.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public class L4W44CombinatorHunt {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            if (v instanceof Object[] a) {
                out = Arrays.deepToString(a);
            } else if (v instanceof int[] a) {
                out = Arrays.toString(a);
            } else if (v instanceof long[] a) {
                out = Arrays.toString(a);
            } else {
                out = String.valueOf(v);
            }
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static final MethodHandles.Lookup L = MethodHandles.lookup();

    static String cat(String a, String b) {
        return a + b;
    }

    static int sum(int... xs) {
        int s = 0;
        for (int x : xs) {
            s += x;
        }
        return s;
    }

    static String join(String sep, Object... xs) {
        StringBuilder b = new StringBuilder();
        for (Object x : xs) {
            if (b.length() > 0) {
                b.append(sep);
            }
            b.append(x);
        }
        return b.toString();
    }

    static String caseA(int x) {
        return "A" + x;
    }

    static String caseB(int x) {
        return "B" + x;
    }

    static String dflt(int x) {
        return "D" + x;
    }

    static int field = 5;

    public static void main(String[] args) throws Throwable {
        // arrayConstructor / arrayLength / arrayElementVarHandle
        row("arrayConstructor-int", () -> {
            MethodHandle h = MethodHandles.arrayConstructor(int[].class);
            int[] a = (int[]) h.invokeExact(3);
            return h.type() + " " + a.length;
        });
        row("arrayConstructor-string2d", () -> {
            MethodHandle h = MethodHandles.arrayConstructor(String[][].class);
            return h.type() + " " + ((String[][]) h.invokeExact(2)).length;
        });
        row("arrayConstructor-negative", () -> (Object) MethodHandles.arrayConstructor(int[].class).invoke(-1));
        row("arrayConstructor-notarray", () -> MethodHandles.arrayConstructor(String.class));
        row("arrayConstructor-null", () -> MethodHandles.arrayConstructor(null));
        row("arrayLength-long", () -> {
            MethodHandle h = MethodHandles.arrayLength(long[].class);
            return h.type() + " " + (int) h.invokeExact(new long[7]);
        });
        row("arrayLength-object", () -> {
            MethodHandle h = MethodHandles.arrayLength(Object[].class);
            return h.type() + " " + (int) h.invokeExact(new Object[] {1, 2});
        });
        row("arrayLength-null-array", () -> (int) MethodHandles.arrayLength(int[].class).invokeExact((int[]) null));
        row("arrayLength-notarray", () -> MethodHandles.arrayLength(int.class));
        row("arrayElementVarHandle-int", () -> {
            VarHandle v = MethodHandles.arrayElementVarHandle(int[].class);
            int[] a = {1, 2, 3};
            v.set(a, 1, 20);
            int old = (int) v.getAndAdd(a, 2, 5);
            return v.varType() + " " + v.coordinateTypes() + " " + Arrays.toString(a) + " " + old;
        });
        row("arrayElementVarHandle-cas-string", () -> {
            VarHandle v = MethodHandles.arrayElementVarHandle(String[].class);
            String[] a = {"x", "y"};
            boolean ok = v.compareAndSet(a, 0, "x", "z");
            boolean no = v.compareAndSet(a, 1, "q", "w");
            return ok + " " + no + " " + Arrays.toString(a);
        });
        row("arrayElementVarHandle-oob", () -> {
            VarHandle v = MethodHandles.arrayElementVarHandle(int[].class);
            return (int) v.get(new int[2], 2);
        });
        row("arrayElementVarHandle-store-check", () -> {
            VarHandle v = MethodHandles.arrayElementVarHandle(Object[].class);
            Object[] a = new String[1];
            v.set(a, 0, (Object) Integer.valueOf(1));
            return a[0];
        });
        row("arrayElementVarHandle-notarray", () -> MethodHandles.arrayElementVarHandle(String.class));
        // tableSwitch
        MethodHandle da = L.findStatic(L4W44CombinatorHunt.class, "dflt", MethodType.methodType(String.class, int.class));
        MethodHandle ca = L.findStatic(L4W44CombinatorHunt.class, "caseA", MethodType.methodType(String.class, int.class));
        MethodHandle cb = L.findStatic(L4W44CombinatorHunt.class, "caseB", MethodType.methodType(String.class, int.class));
        row("tableSwitch", () -> {
            MethodHandle t = MethodHandles.tableSwitch(da, ca, cb);
            return t.type() + " " + (String) t.invokeExact(0) + " " + (String) t.invokeExact(1) + " "
                    + (String) t.invokeExact(2) + " " + (String) t.invokeExact(-1);
        });
        row("tableSwitch-no-cases", () -> {
            MethodHandle t = MethodHandles.tableSwitch(da);
            return (String) t.invokeExact(0);
        });
        row("tableSwitch-type-mismatch", () -> MethodHandles.tableSwitch(da,
                MethodHandles.dropArguments(MethodHandles.constant(String.class, "k"), 0, long.class)));
        row("tableSwitch-bad-leading", () -> MethodHandles.tableSwitch(
                MethodHandles.constant(String.class, "k")));
        row("tableSwitch-null-case", () -> MethodHandles.tableSwitch(da, ca, null));
        // countedLoop / iteratedLoop / whileLoop
        row("countedLoop", () -> {
            MethodHandle body = MethodHandles.dropArguments(
                    L.findStatic(Integer.class, "sum", MethodType.methodType(int.class, int.class, int.class)), 2,
                    int.class);
            // body(v, i, n) = v + i ; init(n) = 0
            MethodHandle loop = MethodHandles.countedLoop(MethodHandles.identity(int.class),
                    MethodHandles.dropArguments(MethodHandles.constant(int.class, 0), 0, int.class), body);
            return loop.type() + " " + (int) loop.invokeExact(5);
        });
        row("countedLoop-range", () -> {
            MethodHandle body = L.findStatic(Integer.class, "sum", MethodType.methodType(int.class, int.class, int.class));
            MethodHandle loop = MethodHandles.countedLoop(MethodHandles.constant(int.class, 3),
                    MethodHandles.constant(int.class, 7), MethodHandles.constant(int.class, 100), body);
            return loop.type() + " " + (int) loop.invokeExact();
        });
        row("countedLoop-void", () -> {
            List<Integer> seen = new ArrayList<>();
            MethodHandle add = L.findVirtual(List.class, "add", MethodType.methodType(boolean.class, Object.class))
                    .bindTo(seen);
            MethodHandle body = MethodHandles.dropReturn(add.asType(MethodType.methodType(boolean.class, int.class)));
            MethodHandle loop = MethodHandles.countedLoop(MethodHandles.constant(int.class, 3), null, body);
            loop.invoke();
            return loop.type() + " " + seen;
        });
        row("iteratedLoop", () -> {
            MethodHandle body = L.findStatic(L4W44CombinatorHunt.class, "cat",
                    MethodType.methodType(String.class, String.class, String.class));
            // body(v, t, iterable) ; init(iterable) = ""
            MethodHandle loop = MethodHandles.iteratedLoop(null,
                    MethodHandles.dropArguments(MethodHandles.constant(String.class, ""), 0, Iterable.class),
                    MethodHandles.dropArguments(body, 2, Iterable.class));
            return loop.type() + " " + (String) loop.invokeExact((Iterable<?>) List.of("a", "b", "c"));
        });
        row("iteratedLoop-iterator", () -> {
            // iterator(List) = list.iterator(); body(v, t) = v + t
            MethodHandle it = L.findVirtual(List.class, "iterator", MethodType.methodType(java.util.Iterator.class));
            MethodHandle body = L.findStatic(L4W44CombinatorHunt.class, "cat",
                    MethodType.methodType(String.class, String.class, String.class));
            MethodHandle loop = MethodHandles.iteratedLoop(it, null,
                    body.asType(MethodType.methodType(String.class, String.class, Object.class)));
            return loop.type() + " " + (String) loop.invokeExact(List.of("x", "y"));
        });
        row("whileLoop", () -> {
            // v starts at 1, doubles while v < 50
            MethodHandle lt = MethodHandles.insertArguments(L.findStatic(L4W44CombinatorHunt.class, "lt",
                    MethodType.methodType(boolean.class, int.class, int.class)), 1, 50);
            MethodHandle dbl = L.findStatic(L4W44CombinatorHunt.class, "twice", MethodType.methodType(int.class, int.class));
            MethodHandle loop = MethodHandles.whileLoop(MethodHandles.constant(int.class, 1), lt, dbl);
            return loop.type() + " " + (int) loop.invokeExact();
        });
        row("doWhileLoop", () -> {
            MethodHandle lt = MethodHandles.insertArguments(L.findStatic(L4W44CombinatorHunt.class, "lt",
                    MethodType.methodType(boolean.class, int.class, int.class)), 1, 0);
            MethodHandle dbl = L.findStatic(L4W44CombinatorHunt.class, "twice", MethodType.methodType(int.class, int.class));
            MethodHandle loop = MethodHandles.doWhileLoop(MethodHandles.constant(int.class, 3), dbl, lt);
            return (int) loop.invokeExact();
        });
        row("whileLoop-bad-pred", () -> MethodHandles.whileLoop(MethodHandles.constant(int.class, 1),
                MethodHandles.identity(int.class), MethodHandles.identity(int.class)));
        row("whileLoop-short-clauses", () -> {
            // loop (int limit)int: init() = 1, pred(v) = v < 20 (ignores limit),
            // body(v, limit) = v + limit
            MethodHandle lt = MethodHandles.insertArguments(L.findStatic(L4W44CombinatorHunt.class, "lt",
                    MethodType.methodType(boolean.class, int.class, int.class)), 1, 20);
            MethodHandle add = L.findStatic(Integer.class, "sum", MethodType.methodType(int.class, int.class, int.class));
            MethodHandle loop = MethodHandles.whileLoop(MethodHandles.constant(int.class, 1), lt, add);
            return loop.type() + " " + (int) loop.invokeExact(7);
        });
        row("countedLoop-bad-iterations", () -> MethodHandles.countedLoop(MethodHandles.constant(long.class, 3L),
                null, MethodHandles.identity(int.class)));
        row("countedLoop-bad-body", () -> MethodHandles.countedLoop(MethodHandles.constant(int.class, 3),
                null, MethodHandles.identity(int.class)));
        row("countedLoop-null-iterations", () -> MethodHandles.countedLoop(null, null,
                MethodHandles.identity(int.class)));
        row("countedLoop-outer-from-end", () -> {
            // body(v, i) declares no A: the loop takes the iterations handle's (int)
            MethodHandle body = L.findStatic(Integer.class, "sum", MethodType.methodType(int.class, int.class, int.class));
            MethodHandle loop = MethodHandles.countedLoop(MethodHandles.identity(int.class), null, body);
            return loop.type() + " " + (int) loop.invokeExact(4);
        });
        // zero / empty / constant
        row("zero-int", () -> {
            MethodHandle z = MethodHandles.zero(int.class);
            return z.type() + " " + (int) z.invokeExact();
        });
        row("zero-object", () -> {
            MethodHandle z = MethodHandles.zero(String.class);
            return z.type() + " " + (String) z.invokeExact();
        });
        row("zero-void", () -> {
            MethodHandle z = MethodHandles.zero(void.class);
            z.invokeExact();
            return z.type();
        });
        row("empty", () -> {
            MethodHandle e = MethodHandles.empty(MethodType.methodType(double.class, String.class, int.class));
            return e.type() + " " + (double) e.invokeExact("x", 3);
        });
        row("constant-widen", () -> {
            MethodHandle c = MethodHandles.constant(long.class, 7);
            return c.type() + " " + (long) c.invokeExact();
        });
        row("constant-bad", () -> MethodHandles.constant(int.class, "x"));
        row("constant-void", () -> MethodHandles.constant(void.class, null));
        row("constant-null-prim", () -> MethodHandles.constant(int.class, null));
        // withVarargs / asCollector / asSpreader
        MethodHandle sum = L.findStatic(L4W44CombinatorHunt.class, "sum", MethodType.methodType(int.class, int[].class));
        MethodHandle join = L.findStatic(L4W44CombinatorHunt.class, "join",
                MethodType.methodType(String.class, String.class, Object[].class));
        row("withVarargs", () -> {
            MethodHandle v = sum.withVarargs(true);
            return v.isVarargsCollector() + " " + (int) v.invoke(1, 2, 3) + " " + (int) v.invoke();
        });
        row("withVarargs-false", () -> {
            MethodHandle v = sum.withVarargs(true).withVarargs(false);
            return v.isVarargsCollector() + " " + v.type();
        });
        row("withVarargs-notarray", () -> MethodHandles.identity(int.class).withVarargs(true));
        row("asCollector-0", () -> {
            MethodHandle c = sum.asCollector(int[].class, 0);
            return c.type() + " " + (int) c.invokeExact();
        });
        row("asCollector-3", () -> {
            MethodHandle c = sum.asCollector(int[].class, 3);
            return c.type() + " " + (int) c.invokeExact(1, 2, 3);
        });
        row("asCollector-pos", () -> {
            MethodHandle c = join.asCollector(1, Object[].class, 2);
            return c.type() + " " + (String) c.invokeExact("-", (Object) "a", (Object) "b");
        });
        row("asCollector-neg", () -> sum.asCollector(int[].class, -1));
        row("asCollector-wrongtype", () -> sum.asCollector(long[].class, 2));
        row("asCollector-toomany", () -> sum.asCollector(int[].class, 256));
        row("asSpreader-0", () -> {
            MethodHandle s = MethodHandles.constant(String.class, "k").asSpreader(Object[].class, 0);
            return s.type() + " " + (String) s.invokeExact((Object[]) null);
        });
        row("asSpreader-2", () -> {
            MethodHandle cat = L.findStatic(L4W44CombinatorHunt.class, "cat",
                    MethodType.methodType(String.class, String.class, String.class));
            MethodHandle s = cat.asSpreader(String[].class, 2);
            return s.type() + " " + (String) s.invokeExact(new String[] {"a", "b"});
        });
        row("asSpreader-wronglen", () -> {
            MethodHandle cat = L.findStatic(L4W44CombinatorHunt.class, "cat",
                    MethodType.methodType(String.class, String.class, String.class));
            return (String) cat.asSpreader(String[].class, 2).invokeExact(new String[] {"a"});
        });
        row("asSpreader-pos", () -> {
            MethodHandle cat = L.findStatic(L4W44CombinatorHunt.class, "cat",
                    MethodType.methodType(String.class, String.class, String.class));
            MethodHandle s = cat.asSpreader(0, Object[].class, 1);
            return s.type() + " " + (String) s.invokeExact(new Object[] {"x"}, "y");
        });
        row("asSpreader-toomany", () -> MethodHandles.constant(String.class, "k").asSpreader(Object[].class, 1));
        row("asSpreader-neg", () -> MethodHandles.constant(String.class, "k").asSpreader(Object[].class, -1));
        // VarHandle.withInvokeExactBehavior
        row("vh-exact-behavior", () -> {
            VarHandle v = L.findStaticVarHandle(L4W44CombinatorHunt.class, "field", int.class);
            VarHandle e = v.withInvokeExactBehavior();
            return v.hasInvokeExactBehavior() + " " + e.hasInvokeExactBehavior() + " " + (int) e.get() + " "
                    + e.withInvokeBehavior().hasInvokeExactBehavior();
        });
        row("vh-exact-mismatch", () -> {
            VarHandle e = L.findStaticVarHandle(L4W44CombinatorHunt.class, "field", int.class)
                    .withInvokeExactBehavior();
            return (long) e.get();
        });
        row("vh-exact-set-boxed", () -> {
            VarHandle e = L.findStaticVarHandle(L4W44CombinatorHunt.class, "field", int.class)
                    .withInvokeExactBehavior();
            e.set((Integer) 6);
            return field;
        });
        row("vh-exact-array", () -> {
            VarHandle e = MethodHandles.arrayElementVarHandle(int[].class).withInvokeExactBehavior();
            int[] a = {4};
            return (int) e.get(a, 0) + " " + e.hasInvokeExactBehavior();
        });
        row("vh-exact-array-mismatch", () -> {
            VarHandle e = MethodHandles.arrayElementVarHandle(int[].class).withInvokeExactBehavior();
            return (int) e.get((Object) new int[] {4}, 0);
        });
    }

    static boolean lt(int a, int b) {
        return a < b;
    }

    static int twice(int a) {
        return a * 2;
    }
}
