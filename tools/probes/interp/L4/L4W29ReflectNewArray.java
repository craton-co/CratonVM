// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L4: `Array.newInstance(Class, int)`,
// `Array.newInstance(Class, int...)` and `Class.arrayType()`. In both modes
// the registered `Bridge`s `Array.newInstance` (`native-builtins/src/lib.rs`
// `native_array_new_instance*`) and `Class.arrayType`
// (`lang_class.rs::native_class_array_type`) answer at the invoke doors'
// step 1; a door that honours the JDK bytecode instead reaches the natives
// the JDK bodies call, `Array.newArray` / `Array.multiNewArray`
// (`lang_system.rs::native_array_new_array` / `native_array_multi_new_array`,
// which the multi-dimensional `newInstance` Bridge also delegates to). All of
// them now make HotSpot's checks in HotSpot's order.
//
// HotSpot's order (`Reflection::reflect_new_array` /
// `reflect_new_multi_array`, measured here): a null component (or a null dimensions array) is a
// NullPointerException before anything else; zero or more than 255
// dimensions is an IllegalArgumentException; a negative length (each
// dimension in order) is a NegativeArraySizeException; then `void`, or an
// array component whose dimensions plus the new ones exceed 255, is an
// IllegalArgumentException. None of them has a message. Neither form
// initializes the component class: there is no "Lazy <clinit> ran" line.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W29ReflectNewArray
//
// HotSpot 25 (25.0.3, default and -Xint) prints exactly:
//   1 null class: java.lang.NullPointerException msg=null
//   2 null class negative: java.lang.NullPointerException msg=null
//   3 void: java.lang.IllegalArgumentException msg=null
//   4 void negative: java.lang.NegativeArraySizeException msg=-1
//   5 int negative: java.lang.NegativeArraySizeException msg=-1
//   6 of 254-dim: 255x[I len=1
//   7 of 255-dim: java.lang.IllegalArgumentException msg=null
//   8 of 255-dim negative: java.lang.NegativeArraySizeException msg=-1
//   9 dims null class: java.lang.NullPointerException msg=null
//   10 dims null array: java.lang.NullPointerException msg=null
//   11 dims empty: java.lang.IllegalArgumentException msg=null
//   12 dims empty null class: java.lang.NullPointerException msg=null
//   13 dims void: java.lang.IllegalArgumentException msg=null
//   14 dims void one: java.lang.IllegalArgumentException msg=null
//   15 dims negative second: java.lang.NegativeArraySizeException msg=-2
//   16 dims negative first: java.lang.NegativeArraySizeException msg=-3
//   17 dims 255: 255x[I len=0
//   18 dims 256: java.lang.IllegalArgumentException msg=null
//   19 dims of 254-dim x1: 255x[I len=1
//   20 dims of 254-dim x2: java.lang.IllegalArgumentException msg=null
//   21 dims String 2x0: [[Ljava.lang.String; len=2
//   22 dims String 2x3 inner: [Ljava.lang.String; len=3
//   23 dims int[] 2x3: [[[I len=2
//   24 dims int[] 2x3 inner: [[I len=3
//   25 dims String[] 2x3: [[[Ljava.lang.String; len=2
//   26 dims long one: [J len=3
//   27 dims negative one: java.lang.NegativeArraySizeException msg=-1
//   28 dims void negative: java.lang.NegativeArraySizeException msg=-1
//   29 dims null class negative: java.lang.NullPointerException msg=null
//   30 Lazy one-dim: [LL4W29ReflectNewArray$Lazy; len=2
//   31 Lazy two-dim: [[LL4W29ReflectNewArray$Lazy; len=2
//   32 Lazy two-dim inner: [LL4W29ReflectNewArray$Lazy; len=2
//   33 hidden one-dim: component=true len=2
//   34 hidden two-dim: component=true inner=true len=2x3
//   35 hidden two-dim instanceof: true
//   36 void arrayType: java.lang.UnsupportedOperationException msg=java.lang.IllegalArgumentException
//   37 Lazy still uninitialized: done
//
// Before wave 29, read from the code. Through the `newInstance` Bridges (both
// modes): row 4 was an IllegalArgumentException (`void` was checked before
// the length), rows 7 / 18 / 20 built arrays of more than 255 dimensions,
// row 10 answered `int[0]`, row 28 was an IllegalArgumentException, rows
// 23 / 25 gave the outer array an unresolvable component (the leaf
// descriptor was spelled `L[I;`), rows 31-32 printed "Lazy <clinit> ran"
// first (the leaf was re-resolved by name through
// `ensure_class_initialized`), and row 36 answered null. Through `newArray` /
// `multiNewArray` (a bytecode-honouring door, `--jdk-only`) rows 1 / 9 also
// built `Object` arrays, rows 3 / 13 / 14 built an array over `void`, and
// row 11 answered `int[0]`. Rows 33-35 (a hidden-class component, which has
// no name to resolve by) are a regression guard: the multi-dimensional
// levels now take their array classes from the leaf's defining loader
// (`array_class_id_for_loader`) and the leaf from the mirror itself.

import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;

public class L4W29ReflectNewArray {
    static class Lazy {
        static { System.out.println("Lazy <clinit> ran"); }
    }
    static class Hid {
        int v;
    }

    interface R { Object run() throws Throwable; }

    static void row(String name, R r) {
        String out;
        try {
            Object v = r.run();
            if (v == null) {
                out = "null";
            } else if (v instanceof String s) {
                out = s;
            } else {
                String n = v.getClass().getName();
                if (n.length() > 40) {
                    int dims = 0;
                    while (dims < n.length() && n.charAt(dims) == '[') dims++;
                    n = dims + "x[" + n.substring(dims);
                }
                out = n + " len=" + Array.getLength(v);
            }
        } catch (Throwable t) {
            out = t.getClass().getName() + " msg=" + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] a) throws Throwable {
        Class<?> d254 = Array.newInstance(int.class, new int[254]).getClass();
        Class<?> d255 = Array.newInstance(int.class, new int[255]).getClass();
        row("1 null class", () -> Array.newInstance(null, 1));
        row("2 null class negative", () -> Array.newInstance(null, -1));
        row("3 void", () -> Array.newInstance(void.class, 1));
        row("4 void negative", () -> Array.newInstance(void.class, -1));
        row("5 int negative", () -> Array.newInstance(int.class, -1));
        row("6 of 254-dim", () -> Array.newInstance(d254, 1));
        row("7 of 255-dim", () -> Array.newInstance(d255, 1));
        row("8 of 255-dim negative", () -> Array.newInstance(d255, -1));
        row("9 dims null class", () -> Array.newInstance(null, 1, 1));
        row("10 dims null array", () -> Array.newInstance(int.class, (int[]) null));
        row("11 dims empty", () -> Array.newInstance(int.class, new int[0]));
        row("12 dims empty null class", () -> Array.newInstance(null, new int[0]));
        row("13 dims void", () -> Array.newInstance(void.class, 1, 1));
        row("14 dims void one", () -> Array.newInstance(void.class, new int[] {1}));
        row("15 dims negative second", () -> Array.newInstance(int.class, 1, -2));
        row("16 dims negative first", () -> Array.newInstance(int.class, -3, 2));
        row("17 dims 255", () -> Array.newInstance(int.class, new int[255]));
        row("18 dims 256", () -> Array.newInstance(int.class, new int[256]));
        row("19 dims of 254-dim x1", () -> Array.newInstance(d254, new int[] {1}));
        row("20 dims of 254-dim x2", () -> Array.newInstance(d254, new int[] {1, 1}));
        row("21 dims String 2x0", () -> Array.newInstance(String.class, 2, 0));
        row("22 dims String 2x3 inner", () -> ((Object[]) Array.newInstance(String.class, 2, 3))[1]);
        row("23 dims int[] 2x3", () -> Array.newInstance(int[].class, 2, 3));
        row("24 dims int[] 2x3 inner", () -> ((Object[]) Array.newInstance(int[].class, 2, 3))[1]);
        row("25 dims String[] 2x3", () -> Array.newInstance(String[].class, 2, 3));
        row("26 dims long one", () -> Array.newInstance(long.class, new int[] {3}));
        row("27 dims negative one", () -> Array.newInstance(long.class, new int[] {-1}));
        row("28 dims void negative", () -> Array.newInstance(void.class, 1, -1));
        row("29 dims null class negative", () -> Array.newInstance(null, 1, -1));
        // Neither form initializes the component class (HotSpot: no <clinit>).
        row("30 Lazy one-dim", () -> Array.newInstance(Lazy.class, 2));
        row("31 Lazy two-dim", () -> Array.newInstance(Lazy.class, 2, 2));
        row("32 Lazy two-dim inner", () -> ((Object[]) Array.newInstance(Lazy.class, 2, 2))[0]);
        // A hidden class has no name to resolve by: the component is the class itself.
        byte[] bytes;
        try (var in = L4W29ReflectNewArray.class.getResourceAsStream("L4W29ReflectNewArray$Hid.class")) {
            bytes = in.readAllBytes();
        }
        Class<?> hid = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
        row("33 hidden one-dim", () -> {
            Object x = Array.newInstance(hid, 2);
            return "component=" + (x.getClass().getComponentType() == hid) + " len=" + Array.getLength(x);
        });
        row("34 hidden two-dim", () -> {
            Object x = Array.newInstance(hid, 2, 3);
            Object inner = ((Object[]) x)[1];
            return "component=" + (x.getClass().getComponentType() == hid.arrayType())
                    + " inner=" + (inner.getClass().getComponentType() == hid)
                    + " len=" + Array.getLength(x) + "x" + Array.getLength(inner);
        });
        row("35 hidden two-dim instanceof", () -> {
            Object x = Array.newInstance(hid, 1, 1);
            return String.valueOf(hid.arrayType().arrayType().isInstance(x));
        });
        // `Class.arrayType` is `Array.newInstance(this, 0).getClass()`, turning
        // the IllegalArgumentException into an UnsupportedOperationException.
        row("36 void arrayType", () -> void.class.arrayType());
        row("37 Lazy still uninitialized", () -> "done");
    }
}
