// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L4: `SwitchBootstraps.typeSwitch` /
// `enumSwitch` shapes javac 25 emits, which CratonVM links natively
// (`invokedynamic.rs::bootstrap_type_switch` / `bootstrap_enum_switch`,
// `execute_type_switch`): qualified enum constants from two enums of a sealed
// hierarchy, constant bodies (an enum constant whose class is a subclass),
// guards that restart the search, boxed `Character` / `Integer` / `Byte`
// selectors with constant labels, `case null` alone and combined with
// `default`, array type patterns, and an enum selector with a type pattern.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W23SwitchBootstraps
//
// HotSpot 25 (25.0.3) prints:
//   sealed A: E1.A
//   sealed b!: E1.B
//   sealed A: E2.A
//   sealed C: other E2 C
//   sealed Rec[v=3]: rec 3
//   sealed Rec[v=30]: big rec 30
//   guards: null
//   guards: long string
//   guards: empty string
//   guards: string ab
//   guards: big int
//   guards: int 5
//   guards: int[] 2
//   guards: String[] 3
//   guards: Object[] 4
//   guards: default Double
//   chars a: a
//   chars b: b or c
//   chars c: b or c
//   chars 7: digit 7
//   chars z: other z
//   bytes 1: one
//   bytes -1: minus one
//   bytes 60: big
//   bytes 9: byte 9
//   ints null: null or default
//   ints 7: seven
//   ints -3: negative
//   ints 12: null or default
//   strings x: x
//   strings yes: y-ish
//   strings null: null
//   strings zz: other zz
//   chars null: java.lang.NullPointerException: null
//   enum A: A
//   enum B: second
//   enum null: java.lang.NullPointerException: null
//
// A regression guard, not a fix: read against the linker it matches HotSpot
// on every row (the `Byte` / `Character` targets reach the `Integer`
// constant labels through `unbox_int`, the qualified enum labels are
// `EnumDesc` condys, the enum selector's class labels link as a type switch).

public class L4W23SwitchBootstraps {
    sealed interface S permits E1, E2, Rec {}
    enum E1 implements S { A, B { @Override public String toString() { return "b!"; } } }
    enum E2 implements S { A, C }
    record Rec(int v) implements S {}

    static String sealedSwitch(S s) {
        return switch (s) {
            case E1.A -> "E1.A";
            case E2.A -> "E2.A";
            case E1.B -> "E1.B";
            case E1 e -> "other E1 " + e;
            case E2 e -> "other E2 " + e;
            case Rec(int v) when v > 10 -> "big rec " + v;
            case Rec r -> "rec " + r.v();
        };
    }

    static String guards(Object o) {
        return switch (o) {
            case null -> "null";
            case String s when s.length() > 3 -> "long string";
            case String s when s.isEmpty() -> "empty string";
            case String s -> "string " + s;
            case Integer i when i > 100 -> "big int";
            case Integer i -> "int " + i;
            case int[] ia -> "int[] " + ia.length;
            case String[] sa -> "String[] " + sa.length;
            case Object[] oa -> "Object[] " + oa.length;
            default -> "default " + o.getClass().getSimpleName();
        };
    }

    static String chars(Character c) {
        return switch (c) {
            case 'a' -> "a";
            case 'b', 'c' -> "b or c";
            case Character x when Character.isDigit(x) -> "digit " + x;
            case Character x -> "other " + x;
        };
    }

    static String bytes(Byte b) {
        return switch (b) {
            case 1 -> "one";
            case -1 -> "minus one";
            case Byte x when x > 50 -> "big";
            case Byte x -> "byte " + x;
        };
    }

    static String ints(Integer i) {
        return switch (i) {
            case 7 -> "seven";
            case Integer x when x < 0 -> "negative";
            case null, default -> "null or default";
        };
    }

    static String strings(String s) {
        return switch (s) {
            case "x" -> "x";
            case String t when t.startsWith("y") -> "y-ish";
            case null -> "null";
            case String t -> "other " + t;
        };
    }

    static String enumWithPattern(E1 e) {
        return switch (e) {
            case A -> "A";
            case E1 x when x.ordinal() == 1 -> "second";
            case E1 x -> "rest";
        };
    }

    static String row(java.util.function.Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        for (S s : new S[] { E1.A, E1.B, E2.A, E2.C, new Rec(3), new Rec(30) }) {
            System.out.println("sealed " + s + ": " + row(() -> sealedSwitch(s)));
        }
        for (Object o : new Object[] { null, "abcd", "", "ab", 500, 5, new int[2], new String[3], new Integer[4], 2.5 }) {
            System.out.println("guards: " + row(() -> guards(o)));
        }
        for (char c : "abc7z".toCharArray()) {
            System.out.println("chars " + c + ": " + row(() -> chars(c)));
        }
        for (byte b : new byte[] { 1, -1, 60, 9 }) {
            System.out.println("bytes " + b + ": " + row(() -> bytes(b)));
        }
        for (Integer i : new Integer[] { null, 7, -3, 12 }) {
            System.out.println("ints " + i + ": " + row(() -> ints(i)));
        }
        for (String s : new String[] { "x", "yes", null, "zz" }) {
            System.out.println("strings " + s + ": " + row(() -> strings(s)));
        }
        System.out.println("chars null: " + row(() -> chars(null)));
        for (E1 e : E1.values()) {
            System.out.println("enum " + e.name() + ": " + row(() -> enumWithPattern(e)));
        }
        System.out.println("enum null: " + row(() -> enumWithPattern(null)));
    }
}
