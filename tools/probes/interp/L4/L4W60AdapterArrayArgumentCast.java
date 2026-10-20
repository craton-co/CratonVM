// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L4 --
// docs/internal/fixed-bugs/interpreter-L4-an-adapted-method-handle-passes-an-unchecked-reference-argument-FIXED-20261024.md:
// every adapter door that converts a reference argument (or return) to an
// ARRAY type must make the `checkcast` `asType` implies -- component
// included. CratonVM judged only the array's shape (array or not, primitive
// or not), so an `Integer[]` reached a `String[]` parameter through `asType`,
// `asSpreader`, `invoke`, `invokeWithArguments`, a collector, a filter's
// return, a field setter, an array-element setter, `bindTo`,
// `insertArguments` and `constant`. Rows per door: a refused array, an admitted
// covariant / interface-component array, `null` (always passes) and the
// generic-erasure shapes HotSpot accepts. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// astype-integers-to-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// astype-strings: strings String[]#1
// astype-null: strings null
// astype-objects-to-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.Object; to [Ljava.lang.String;
// astype-int-array-to-strings: java.lang.ClassCastException Cannot cast [I to [Ljava.lang.String;
// astype-strings-to-chars: chars String[]#1
// astype-integers-to-chars: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.CharSequence;
// astype-integers-to-comparables: comparables Integer[]#1
// astype-integers-to-objects: objects Integer[]#1
// astype-int-array-to-objects: java.lang.ClassCastException Cannot cast [I to [Ljava.lang.Object;
// astype-integerss-to-stringss: java.lang.ClassCastException Cannot cast [[Ljava.lang.Integer; to [[Ljava.lang.String;
// astype-stringss: multi String[][]#1
// astype-strings-to-stringss: java.lang.ClassCastException Cannot cast [Ljava.lang.String; to [[Ljava.lang.String;
// astype-erased-arraylists-to-lists: lists ArrayList[]#1
// astype-erased-integer-list: list ArrayList
// explicit-integers-to-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// explicit-integers-to-chars: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.CharSequence;
// explicit-null: strings null
// invoke-integers-to-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// invoke-strings: strings String[]#1
// invoke-objects-to-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.Object; to [Ljava.lang.String;
// invoke-with-arguments-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// invoke-with-arguments-null: strings null
// spreader-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// spreader-strings: strings String[]#1
// collector-integers-element: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// collector-strings-element: multi String[][]#1
// collector-null-element: multi String[][]#1
// varargs-integers-as-object: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to java.lang.String
// filter-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// filter-strings: strings String[]#1
// filter-null: strings null
// return-integers-to-strings: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// return-integers: Integer[]#1
// setter-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// setter-strings: set String[]#1
// array-setter-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// array-setter-strings: stored String[]#1
// array-getter-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// array-getter-strings: a
// bindto-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// insert-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// insert-strings: strings String[]#1
// constant-integers: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// constant-strings: String[]#1
// astype-integers-to-strings-again: java.lang.ClassCastException Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W60AdapterArrayArgumentCast`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L4W60AdapterArrayArgumentCast {
    static String[] sf;

    static String desc(Object a) {
        if (a == null) {
            return "null";
        }
        if (a instanceof Object[] arr) {
            return a.getClass().getSimpleName() + "#" + arr.length;
        }
        return a.getClass().getSimpleName();
    }

    public static String strings(String[] a) {
        return "strings " + desc(a);
    }

    public static String chars(CharSequence[] a) {
        return "chars " + desc(a);
    }

    public static String comparables(Comparable<?>[] a) {
        return "comparables " + desc(a);
    }

    public static String objects(Object[] a) {
        return "objects " + desc(a);
    }

    public static String multi(String[][] a) {
        return "multi " + desc(a);
    }

    public static String lists(List<String>[] a) {
        return "lists " + desc(a);
    }

    public static String list(List<String> l) {
        return "list " + desc(l);
    }

    public static String varargs(String... a) {
        return "varargs " + desc(a);
    }

    public static Object returnsInts() {
        return new Integer[] {1};
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            Object v = r.run();
            System.out.println(name + ": " + (v instanceof String ? v : desc(v)));
        } catch (Throwable t) {
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage());
        }
    }

    static final MethodType OO = MethodType.methodType(Object.class, Object.class);

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W60AdapterArrayArgumentCast.class;
        MethodHandle strings = l.findStatic(me, "strings", MethodType.methodType(String.class, String[].class));
        MethodHandle chars = l.findStatic(me, "chars", MethodType.methodType(String.class, CharSequence[].class));
        MethodHandle comparables =
                l.findStatic(me, "comparables", MethodType.methodType(String.class, Comparable[].class));
        MethodHandle objects = l.findStatic(me, "objects", MethodType.methodType(String.class, Object[].class));
        MethodHandle multi = l.findStatic(me, "multi", MethodType.methodType(String.class, String[][].class));
        MethodHandle lists = l.findStatic(me, "lists", MethodType.methodType(String.class, List[].class));
        MethodHandle list = l.findStatic(me, "list", MethodType.methodType(String.class, List.class));
        MethodHandle varargs = l.findStatic(me, "varargs", MethodType.methodType(String.class, String[].class));
        MethodHandle returnsInts = l.findStatic(me, "returnsInts", MethodType.methodType(Object.class));
        MethodHandle setter = l.findStaticSetter(me, "sf", String[].class);

        Object ints = new Integer[] {1};
        Object strs = new String[] {"a"};
        Object objs = new Object[] {"a"};
        Object prims = new int[] {1};
        Object intss = new Integer[][] {{1}};
        Object strss = new String[][] {{"a"}};
        @SuppressWarnings("unchecked")
        Object arrayLists = new ArrayList[] {new ArrayList<Integer>()};
        List<Integer> rawInts = new ArrayList<>();
        rawInts.add(1);
        Object rawList = rawInts;

        // asType
        MethodHandle gStrings = strings.asType(OO);
        row("astype-integers-to-strings", () -> gStrings.invokeExact(ints));
        row("astype-strings", () -> gStrings.invokeExact(strs));
        row("astype-null", () -> gStrings.invokeExact((Object) null));
        row("astype-objects-to-strings", () -> gStrings.invokeExact(objs));
        row("astype-int-array-to-strings", () -> gStrings.invokeExact(prims));
        MethodHandle gChars = chars.asType(OO);
        row("astype-strings-to-chars", () -> gChars.invokeExact(strs));
        row("astype-integers-to-chars", () -> gChars.invokeExact(ints));
        MethodHandle gComparables = comparables.asType(OO);
        row("astype-integers-to-comparables", () -> gComparables.invokeExact(ints));
        MethodHandle gObjects = objects.asType(OO);
        row("astype-integers-to-objects", () -> gObjects.invokeExact(ints));
        row("astype-int-array-to-objects", () -> gObjects.invokeExact(prims));
        MethodHandle gMulti = multi.asType(OO);
        row("astype-integerss-to-stringss", () -> gMulti.invokeExact(intss));
        row("astype-stringss", () -> gMulti.invokeExact(strss));
        row("astype-strings-to-stringss", () -> gMulti.invokeExact(strs));
        MethodHandle gLists = lists.asType(OO);
        row("astype-erased-arraylists-to-lists", () -> gLists.invokeExact(arrayLists));
        MethodHandle gList = list.asType(OO);
        row("astype-erased-integer-list", () -> gList.invokeExact(rawList));

        // explicitCastArguments: an array type is no interface, so it casts
        MethodHandle eStrings = MethodHandles.explicitCastArguments(strings, OO);
        row("explicit-integers-to-strings", () -> eStrings.invokeExact(ints));
        MethodHandle eChars = MethodHandles.explicitCastArguments(chars, OO);
        row("explicit-integers-to-chars", () -> eChars.invokeExact(ints));
        row("explicit-null", () -> eStrings.invokeExact((Object) null));

        // invoke / invokeWithArguments
        row("invoke-integers-to-strings", () -> (Object) strings.invoke(ints));
        row("invoke-strings", () -> (Object) strings.invoke(strs));
        row("invoke-objects-to-strings", () -> (Object) strings.invoke(objs));
        row("invoke-with-arguments-integers", () -> strings.invokeWithArguments(new Object[] {ints}));
        row("invoke-with-arguments-null", () -> strings.invokeWithArguments(new Object[] {null}));

        // spreader / collector
        MethodHandle spread = strings.asSpreader(Object[].class, 1).asType(
                MethodType.methodType(Object.class, Object[].class));
        row("spreader-integers", () -> spread.invokeExact(new Object[] {ints}));
        row("spreader-strings", () -> spread.invokeExact(new Object[] {strs}));
        MethodHandle collect = multi.asCollector(String[][].class, 1).asType(OO);
        row("collector-integers-element", () -> collect.invokeExact(ints));
        row("collector-strings-element", () -> collect.invokeExact(strs));
        row("collector-null-element", () -> collect.invokeExact((Object) null));
        row("varargs-integers-as-object", () -> (Object) varargs.invoke(ints));

        // filterArguments: the filter's own asType'd return is the boundary
        MethodHandle toStrings = MethodHandles.identity(Object.class)
                .asType(MethodType.methodType(String[].class, Object.class));
        MethodHandle filtered = MethodHandles.filterArguments(strings, 0, toStrings);
        row("filter-integers", () -> (String) filtered.invokeExact(ints));
        row("filter-strings", () -> (String) filtered.invokeExact(strs));
        row("filter-null", () -> (String) filtered.invokeExact((Object) null));

        // A return cast to an array type
        MethodHandle ret = returnsInts.asType(MethodType.methodType(String[].class));
        row("return-integers-to-strings", () -> (Object) (String[]) ret.invokeExact());
        MethodHandle retOk = returnsInts.asType(MethodType.methodType(Integer[].class));
        row("return-integers", () -> (Object) (Integer[]) retOk.invokeExact());

        // A field setter
        MethodHandle set = setter.asType(MethodType.methodType(void.class, Object.class));
        row("setter-integers", () -> {
            set.invokeExact(ints);
            return "set " + desc(sf);
        });
        row("setter-strings", () -> {
            set.invokeExact(strs);
            return "set " + desc(sf);
        });

        // An array-element setter whose component is itself an array
        MethodHandle aset = MethodHandles.arrayElementSetter(String[][].class);
        Object grid = new String[1][];
        row("array-setter-integers", () -> {
            aset.invoke(grid, 0, ints);
            return "stored " + desc(((String[][]) grid)[0]);
        });
        row("array-setter-strings", () -> {
            aset.invoke(grid, 0, strs);
            return "stored " + desc(((String[][]) grid)[0]);
        });

        MethodHandle aget = MethodHandles.arrayElementGetter(String[].class);
        row("array-getter-integers", () -> (Object) aget.invoke(ints, 0));
        row("array-getter-strings", () -> (Object) aget.invoke(strs, 0));

        // Binding doors
        row("bindto-integers", () -> desc(strings.bindTo(ints)));
        row("insert-integers", () -> desc(MethodHandles.insertArguments(strings, 0, ints)));
        row("insert-strings", () -> (Object) MethodHandles.insertArguments(strings, 0, strs).invoke());
        row("constant-integers", () -> desc(MethodHandles.constant(String[].class, ints)));
        row("constant-strings", () -> desc((Object) MethodHandles.constant(String[].class, strs).invoke()));

        row("astype-integers-to-strings-again", () -> gStrings.invokeExact(ints));
    }
}
