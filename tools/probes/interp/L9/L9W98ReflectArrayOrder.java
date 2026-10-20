// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- which exception java.lang.reflect.Array
// raises when an argument is wrong in more than one way, and its text.
//
// HotSpot's JVM_SetArrayElement UNBOXES the value before Reflection::array_set
// checks the index: a null stored into a primitive array is an
// IllegalArgumentException (no message) even at a bad index, while a non-box
// value at a bad index is an ArrayIndexOutOfBoundsException (it unboxes to
// T_ILLEGAL without throwing; the widening refusal comes after the bounds
// check). CratonVM checked the index first, so `set-null-prim-bad-index`
// said ArrayIndexOutOfBoundsException (fixed in wave 98: native_array_set).
//
//   $J/javac -d out tools/probes/interp/L9/L9W98ReflectArrayOrder.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W98ReflectArrayOrder
//   $J/java -cp out L9W98ReflectArrayOrder
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// set-null-prim-bad-index: java.lang.IllegalArgumentException: null
// set-null-prim-neg-index: java.lang.IllegalArgumentException: null
// set-null-prim: java.lang.IllegalArgumentException: null
// set-string-prim-bad-index: java.lang.ArrayIndexOutOfBoundsException: null
// set-string-prim: java.lang.IllegalArgumentException: argument type mismatch
// set-long-into-int: java.lang.IllegalArgumentException: argument type mismatch
// set-int-into-long: ok
// set-null-ref-bad-index: java.lang.ArrayIndexOutOfBoundsException: null
// set-wrong-ref-bad-index: java.lang.ArrayIndexOutOfBoundsException: null
// set-wrong-ref: java.lang.IllegalArgumentException: array element type mismatch
// set-not-array: java.lang.IllegalArgumentException: Argument is not an array
// set-null-array: java.lang.NullPointerException: null
// get-bad-index: java.lang.ArrayIndexOutOfBoundsException: null
// get-not-array: java.lang.IllegalArgumentException: Argument is not an array
// getint-ref-array-bad-index: java.lang.IllegalArgumentException: Argument is not an array of primitive type
// getint-long-array: java.lang.IllegalArgumentException: argument type mismatch
// getint-long-array-bad-index: java.lang.ArrayIndexOutOfBoundsException: null
// setint-ref-array: java.lang.IllegalArgumentException: Argument is not an array of primitive type
// setint-short-array-bad-index: java.lang.ArrayIndexOutOfBoundsException: null
// setint-short-array: java.lang.IllegalArgumentException: argument type mismatch
// getlength-not-array: java.lang.IllegalArgumentException: Argument is not an array
// getlength-null: java.lang.NullPointerException: null
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Array;

public class L9W98ReflectArrayOrder {
    interface Shape {
        Object run() throws Throwable;
    }

    static void row(String label, Shape s) {
        String text;
        try {
            s.run();
            text = "ok";
        } catch (Throwable t) {
            text = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + ": " + text);
    }

    public static void main(String[] args) {
        int[] ints = new int[4];
        long[] longs = new long[4];
        short[] shorts = new short[4];
        String[] strings = new String[4];
        row("set-null-prim-bad-index", () -> { Array.set(ints, 9, null); return null; });
        row("set-null-prim-neg-index", () -> { Array.set(ints, -1, null); return null; });
        row("set-null-prim", () -> { Array.set(ints, 0, null); return null; });
        row("set-string-prim-bad-index", () -> { Array.set(ints, 9, "x"); return null; });
        row("set-string-prim", () -> { Array.set(ints, 0, "x"); return null; });
        row("set-long-into-int", () -> { Array.set(ints, 0, 1L); return null; });
        row("set-int-into-long", () -> { Array.set(longs, 0, 1); return null; });
        row("set-null-ref-bad-index", () -> { Array.set(strings, 9, null); return null; });
        row("set-wrong-ref-bad-index", () -> { Array.set(strings, 9, 1); return null; });
        row("set-wrong-ref", () -> { Array.set(strings, 0, 1); return null; });
        row("set-not-array", () -> { Array.set("s", 0, null); return null; });
        row("set-null-array", () -> { Array.set(null, 0, 1); return null; });
        row("get-bad-index", () -> Array.get(ints, 4));
        row("get-not-array", () -> Array.get("s", 0));
        row("getint-ref-array-bad-index", () -> Array.getInt(strings, 9));
        row("getint-long-array", () -> Array.getInt(longs, 0));
        row("getint-long-array-bad-index", () -> Array.getInt(longs, 9));
        row("setint-ref-array", () -> { Array.setInt(strings, 0, 1); return null; });
        row("setint-short-array-bad-index", () -> { Array.setInt(shorts, 9, 1); return null; });
        row("setint-short-array", () -> { Array.setInt(shorts, 0, 1); return null; });
        row("getlength-not-array", () -> Array.getLength("s"));
        row("getlength-null", () -> Array.getLength(null));
    }
}
