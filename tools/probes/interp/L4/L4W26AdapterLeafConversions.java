// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 26, lane L4: `asType`'s reference -> primitive
// conversion for an argument that reaches a primitive parameter THROUGH an
// adapter handle (`insertArguments`, `dropArguments`, `asSpreader`), through an
// `asType`-retyped direct handle called with `invokeExact`, and through the
// leaf kinds that did not adapt at all (a `findSpecial` handle, a field
// setter). Page:
// docs/internal/fixed-bugs/interpreter-L4-an-adapter-handle-passes-an-unconvertible-reference-to-a-primitive-parameter-FIXED-20260928.md
//
// Also the placeholders an adapter itself passes (census of the change):
// `tryFinally`'s result slot when the target threw (HotSpot passes the
// primitive's zero) and a loop whose `init` is null or whose body is `void`;
// and `explicitCastArguments`, whose conversion is a cast that refuses none of
// these (rows 16-18: a null is zero, a Long narrows, and the cast does not
// reach a strict handle the leaf's own Java code invokes).
//
// Each row runs WARM times in `drive` (the call site is in the row's lambda,
// so the JIT compiles it); a row prints its first outcome and, if any later
// iteration answered differently, how many did.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W26AdapterLeafConversions
// (all four combinations must print exactly HotSpot's lines)
//
// HotSpot 25 (25.0.3) prints, verbatim:
//   1 insert String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   2 insert Long: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   3 insert Short: 3
//   4 drop String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   5 drop null: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   6 spreader String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   7 insert Integer-typed null: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
//   8 insert withArgs String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   9 insert pair Integer: 1:5:Integer
//   10 special withArgs Integer: 42
//   11 asType exact String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   11b asType exact Short: 5
//   12 setter String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   12b setter Short: 7
//   13 tryFinally throws: java.lang.IllegalStateException: boom 4 cleanup=[IllegalStateException/r=0/x=4]
//   14 countedLoop null init: 3
//   15 countedLoop void body: [0, 1, 2]
//   16 explicitCast null+Long: 5
//   16b explicitCast Double+Character: 67
//   16c explicitCast String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   17 explicitCast insert null: 1
//   18 explicitCast then strict inside: 0 java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   19 spreadInvoker lead 1: ab
//   19b spreadInvoker short array: java.lang.IllegalArgumentException: array is not of length 2
//   19c spreadInvoker null array: java.lang.NullPointerException: null array reference
//   19d spreadInvoker unbox: 3
//   19e spreadInvoker String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   20 invoker Integer to Object: 5:Integer:2
//   21 collector withArgs Short: 3
//   21b collector withArgs String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   21c collector withArgs null: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//
// CratonVM before the fix (read from the code, not run; both modes): rows 1, 2,
// 4-8, 11, 12 and 17 ran the target with the String's address, an unconverted
// Long or a null in an `int` parameter (no exception); row 9 handed `pair`'s
// `Object b` a raw `int` (the `invoke` native unboxed position 0 against the
// LEAF's `int a`); row 10 handed `twice` a boxed `Integer`; row 13's cleanup
// and row 14's body read a null as an `int`; row 15 passed `i` one slot late;
// rows 16, 16b and 18 threw at the outer call (the NPE of row 5, or `Cannot
// cast java.lang.Double to java.lang.Integer`: wave 25's door check judged an
// explicit cast as an `asType`); rows 19-19e packed the trailing arguments
// into a new `Object[]` instead of spreading the one passed (`concat` of an
// array, no length or null check). Row 20 is a control: an invoker's
// arguments reach the target's leaf as passed (its `MH_DESC` is empty). Rows
// 21b and 21c stored the `int` collector's zero for the String and the null
// (the arm unboxed a wrapper and passed anything else to the array store).
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

class L4W26Base {
    int twice(int x) {
        return 2 * x;
    }
}

public class L4W26AdapterLeafConversions extends L4W26Base {
    static final int WARM = 3000;

    interface Body {
        Object run() throws Throwable;
    }

    static int sum(int a, int b) {
        return a + b;
    }

    static String pair(int a, Object b) {
        return a + ":" + b + ":" + (b == null ? "null" : b.getClass().getSimpleName());
    }

    static final class Box {
        int i;
    }

    static int boom(int x) {
        throw new IllegalStateException("boom " + x);
    }

    static int cleanup(Throwable t, int r, int x) {
        CLEANUPS.add((t == null ? "none" : t.getClass().getSimpleName()) + "/r=" + r + "/x=" + x);
        return r;
    }

    static int accumulate(int v, int i) {
        return v + i;
    }

    static final List<String> CLEANUPS = new ArrayList<>();
    static final List<Integer> SEEN = new ArrayList<>();

    static void record(int i) {
        if (SEEN.size() < 8) {
            SEEN.add(i);
        }
    }

    static String outcome(Body b) {
        try {
            Object r = b.run();
            return String.valueOf(r);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void drive(String label, Body b) {
        String first = outcome(b);
        int differing = 0;
        for (int i = 1; i < WARM; i++) {
            if (!first.equals(outcome(b))) {
                differing++;
            }
        }
        System.out.println(label + ": " + first + (differing == 0 ? "" : "  [" + differing + " later iterations differ]"));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType ii = MethodType.methodType(int.class, int.class, int.class);
        MethodHandle s = l.findStatic(L4W26AdapterLeafConversions.class, "sum", ii);
        MethodHandle ins = MethodHandles.insertArguments(s, 0, 1);
        MethodHandle drop = MethodHandles.dropArguments(s, 0, String.class);
        MethodHandle spread = s.asSpreader(Object[].class, 2);

        // The page's six rows.
        drive("1 insert String", () -> (int) ins.invoke((Object) "s"));
        drive("2 insert Long", () -> (int) ins.invoke((Object) Long.valueOf(2)));
        drive("3 insert Short", () -> (int) ins.invoke((Object) Short.valueOf((short) 2)));
        drive("4 drop String", () -> (int) drop.invoke("x", (Object) "s", 2));
        drive("5 drop null", () -> (int) drop.invoke("x", (Object) null, 2));
        drive("6 spreader String", () -> (int) spread.invoke(new Object[] {"s", 2}));

        // The adapter's own call-site type: an Integer-typed null.
        drive("7 insert Integer-typed null", () -> (int) ins.invoke((Integer) null));
        // `invokeWithArguments` on an adapter (the generic door).
        drive("8 insert withArgs String", () -> ins.invokeWithArguments("s"));

        // The invoke native unboxed an adapter's arguments against the LEAF's
        // descriptor, one slot off: position 0 of `insert(pair, 0, 1)` is
        // `pair`'s `Object b`, but it was unboxed as `pair`'s `int a`.
        MethodHandle p = l.findStatic(L4W26AdapterLeafConversions.class, "pair",
                MethodType.methodType(String.class, int.class, Object.class));
        MethodHandle pins = MethodHandles.insertArguments(p, 0, 1);
        drive("9 insert pair Integer", () -> (String) pins.invoke((Object) Integer.valueOf(5)));

        // A `findSpecial` handle took no argument conversion at all.
        MethodHandle sp = l.findSpecial(L4W26Base.class, "twice",
                MethodType.methodType(int.class, int.class), L4W26AdapterLeafConversions.class);
        L4W26AdapterLeafConversions self = new L4W26AdapterLeafConversions();
        drive("10 special withArgs Integer", () -> sp.invokeWithArguments(self, 21));

        // An `asType`-retyped direct handle, called exactly: the conversion is
        // the retyped handle's.
        MethodHandle g = s.asType(MethodType.methodType(int.class, Object.class, Object.class));
        drive("11 asType exact String", () -> (int) g.invokeExact((Object) "s", (Object) 2));
        drive("11b asType exact Short", () -> (int) g.invokeExact((Object) Short.valueOf((short) 3), (Object) 2));

        // A field setter behind an adapter.
        Box box = new Box();
        MethodHandle set = MethodHandles.insertArguments(l.findSetter(Box.class, "i", int.class), 0, box);
        drive("12 setter String", () -> {
            set.invoke((Object) "s");
            return box.i;
        });
        drive("12b setter Short", () -> {
            set.invoke((Object) Short.valueOf((short) 7));
            return box.i;
        });

        // `tryFinally`: the cleanup's result slot, when the target threw, is
        // the primitive's zero.
        MethodHandle tf = MethodHandles.tryFinally(
                l.findStatic(L4W26AdapterLeafConversions.class, "boom", MethodType.methodType(int.class, int.class)),
                l.findStatic(L4W26AdapterLeafConversions.class, "cleanup",
                        MethodType.methodType(int.class, Throwable.class, int.class, int.class)));
        drive("13 tryFinally throws", () -> {
            CLEANUPS.clear();
            String r = outcome(() -> (int) tf.invoke(4));
            return r + " cleanup=" + CLEANUPS;
        });

        // `countedLoop` with a null `init`: the loop variable starts at its zero.
        MethodHandle three = MethodHandles.constant(int.class, 3);
        MethodHandle acc = l.findStatic(L4W26AdapterLeafConversions.class, "accumulate", ii);
        MethodHandle cl = MethodHandles.countedLoop(three, null, acc);
        drive("14 countedLoop null init", () -> (int) cl.invoke());
        // A `void` body takes no loop variable: `body(i)`.
        MethodHandle rec = l.findStatic(L4W26AdapterLeafConversions.class, "record",
                MethodType.methodType(void.class, int.class));
        MethodHandle vl = MethodHandles.countedLoop(three, null, rec);
        drive("15 countedLoop void body", () -> {
            SEEN.clear();
            vl.invoke();
            return SEEN;
        });

        // `explicitCastArguments` casts where `asType` refuses: a null is the
        // zero and a Long narrows -- on a direct handle and on an adapter.
        MethodHandle ec = MethodHandles.explicitCastArguments(s,
                MethodType.methodType(int.class, Object.class, Object.class));
        drive("16 explicitCast null+Long", () -> (int) ec.invoke((Object) null, (Object) Long.valueOf(5)));
        drive("16b explicitCast Double+Character", () -> (int) ec.invoke((Object) Double.valueOf(2.9), (Object) Character.valueOf('A')));
        drive("16c explicitCast String", () -> (int) ec.invoke((Object) "s", (Object) 1));
        MethodHandle eins = MethodHandles.explicitCastArguments(ins,
                MethodType.methodType(int.class, Object.class));
        drive("17 explicitCast insert null", () -> (int) eins.invoke((Object) null));
        // ... and the cast does not reach a strict handle the leaf's own code
        // invokes.
        INS = ins;
        MethodHandle nested = l.findStatic(L4W26AdapterLeafConversions.class, "nested",
                MethodType.methodType(String.class, int.class));
        MethodHandle en = MethodHandles.explicitCastArguments(nested,
                MethodType.methodType(String.class, Object.class));
        drive("18 explicitCast then strict inside", () -> (String) en.invoke((Object) null));

        // `spreadInvoker(type, lead)` spreads its trailing Object[] into the
        // target's arguments (CratonVM packed the trailing arguments into a
        // new array instead), with `asSpreader`'s checks; row 20 is the plain
        // invoker's control.
        MethodHandle cat = l.findVirtual(String.class, "concat",
                MethodType.methodType(String.class, String.class));
        MethodHandle si1 = MethodHandles.spreadInvoker(cat.type(), 1);
        MethodHandle si0 = MethodHandles.spreadInvoker(cat.type(), 0);
        drive("19 spreadInvoker lead 1", () -> (String) si1.invoke(cat, "a", new Object[] {"b"}));
        drive("19b spreadInvoker short array", () -> (String) si0.invoke(cat, new Object[] {"a"}));
        drive("19c spreadInvoker null array", () -> (String) si0.invoke(cat, (Object[]) null));
        MethodHandle sis = MethodHandles.spreadInvoker(s.type(), 1);
        drive("19d spreadInvoker unbox", () -> (int) sis.invoke(s, 1, new Object[] {Short.valueOf((short) 2)}));
        drive("19e spreadInvoker String", () -> (int) sis.invoke(s, 1, new Object[] {"x"}));
        MethodHandle p2 = l.findStatic(L4W26AdapterLeafConversions.class, "pair2",
                MethodType.methodType(String.class, Object.class, int.class));
        MethodHandle inv = MethodHandles.invoker(p2.type());
        drive("20 invoker Integer to Object", () -> (String) inv.invoke(p2, (Object) Integer.valueOf(5), 2));

        // A primitive collector's elements take the same conversion.
        MethodHandle sumAll = l.findStatic(L4W26AdapterLeafConversions.class, "sumAll",
                MethodType.methodType(int.class, int[].class)).asCollector(int[].class, 2);
        drive("21 collector withArgs Short", () -> sumAll.invokeWithArguments(1, Short.valueOf((short) 2)));
        drive("21b collector withArgs String", () -> sumAll.invokeWithArguments(1, "x"));
        drive("21c collector withArgs null", () -> sumAll.invokeWithArguments(1, null));
    }

    static int sumAll(int[] xs) {
        int t = 0;
        for (int x : xs) {
            t += x;
        }
        return t;
    }

    static String pair2(Object a, int b) {
        return a + ":" + (a == null ? "null" : a.getClass().getSimpleName()) + ":" + b;
    }

    static MethodHandle INS;

    static String nested(int x) {
        return x + " " + outcome(() -> (int) INS.invoke((Object) null));
    }
}
