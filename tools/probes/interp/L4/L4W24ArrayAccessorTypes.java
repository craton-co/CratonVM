// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L4: the array accessor handles
// (`MethodHandles.arrayElementGetter` / `arrayElementSetter` /
// `arrayLength`), which CratonVM answers with its own natives in both modes
// (`native-builtins/src/lang_invoke.rs`, `MH_KIND_ARRAY_GET` / `_SET` /
// `_LENGTH`), handed an array of ANOTHER type through `invoke` /
// `invokeWithArguments`, and the stores and index refusals of
// `L4W23ForcedCombinators` that were still wrong after wave 23.
//
// HotSpot casts the array argument to the handle's array type before the
// access, so a mismatch is a `ClassCastException` (before any index check);
// a setter stores through `aastore`, so a value the ARRAY's component refuses
// is an `ArrayStoreException` even when the handle's type admits it.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W24ArrayAccessorTypes
//
// HotSpot 25 (25.0.3) prints:
//   int getter long[]: java.lang.ClassCastException: Cannot cast [J to [I
//   int getter String: java.lang.ClassCastException: Cannot cast java.lang.String to [I
//   String getter Integer[]: java.lang.ClassCastException: Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
//   String getter Object[]: java.lang.ClassCastException: Cannot cast [Ljava.lang.Object; to [Ljava.lang.String;
//   String getter String[][]: java.lang.ClassCastException: Cannot cast [[Ljava.lang.String; to [Ljava.lang.String;
//   CharSequence getter String[]: cs
//   String setter Integer[]: java.lang.ClassCastException: Cannot cast [Ljava.lang.Integer; to [Ljava.lang.String;
//   Object getter int[]: java.lang.ClassCastException: Cannot cast [I to [Ljava.lang.Object;
//   Object getter String[][]: [Ljava.lang.String;
//   int getter oob wrong type: java.lang.ClassCastException: Cannot cast [J to [I
//   int getter withArgs: java.lang.ClassCastException: Cannot cast [J to [I
//   arrayLength long[]: java.lang.ClassCastException: Cannot cast [J to [I
//   getter oob: java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
//   setter wrong component: java.lang.ArrayStoreException: java.lang.Integer
//   setter right component: ok
//   setter null into String[]: null
//
// Before wave 24 (read from the code, not run): every `ClassCastException`
// row answered instead (a truncated or reinterpreted element, `stored`, or --
// for `String getter Integer[]` -- an `Integer` handed to a `String`-typed
// call site); `setter wrong component` printed `stored`; `getter oob` printed
// `java.lang.ArrayIndexOutOfBoundsException: null`.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W24ArrayAccessorTypes {
    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            out = String.valueOf(c.run());
        } catch (Throwable t) {
            out = t.toString();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        MethodHandle gi = MethodHandles.arrayElementGetter(int[].class);
        MethodHandle gs = MethodHandles.arrayElementGetter(String[].class);
        MethodHandle gc = MethodHandles.arrayElementGetter(CharSequence[].class);
        MethodHandle ss = MethodHandles.arrayElementSetter(String[].class);
        MethodHandle so = MethodHandles.arrayElementSetter(Object[].class);
        MethodHandle go = MethodHandles.arrayElementGetter(Object[].class);
        MethodHandle li = MethodHandles.arrayLength(int[].class);
        row("int getter long[]", () -> (int) gi.invoke((Object) new long[] {5}, 0));
        row("int getter String", () -> (int) gi.invoke((Object) "x", 0));
        row("String getter Integer[]", () -> (String) gs.invoke((Object) new Integer[] {1}, 0));
        row("String getter Object[]", () -> (String) gs.invoke((Object) new Object[] {"s"}, 0));
        row("String getter String[][]", () -> (String) gs.invoke((Object) new String[1][1], 0));
        row("CharSequence getter String[]", () -> gc.invoke((Object) new String[] {"cs"}, 0));
        row("String setter Integer[]", () -> {
            ss.invoke((Object) new Integer[1], 0, "v");
            return "stored";
        });
        row("Object getter int[]", () -> go.invoke((Object) new int[1], 0));
        row("Object getter String[][]", () -> go.invoke((Object) new String[][] {{"q"}}, 0).getClass().getName());
        row("int getter oob wrong type", () -> (int) gi.invoke((Object) new long[0], 3));
        row("int getter withArgs", () -> gi.invokeWithArguments(new long[] {5}, 0));
        row("arrayLength long[]", () -> (int) li.invoke((Object) new long[3]));
        row("getter oob", () -> (int) gi.invokeExact(new int[2], 2));
        row("setter wrong component", () -> {
            Object[] a = new String[1];
            so.invokeExact(a, 0, (Object) Integer.valueOf(1));
            return "stored";
        });
        row("setter right component", () -> {
            Object[] a = new String[1];
            so.invokeExact(a, 0, (Object) "ok");
            return a[0];
        });
        row("setter null into String[]", () -> {
            Object[] a = new String[1];
            so.invokeExact(a, 0, (Object) null);
            return a[0];
        });
    }
}
