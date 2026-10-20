// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 15, lane fuzz4: tableswitch / lookupswitch / String /
// enum / pattern switches at their key-range edges, compiled on a narrow
// profile and then fed every key.
//
// Risk targeted: the switch lowering (`CRATONVM_JIT_IR_SWITCH_SEARCH_TREE`,
// `CRATONVM_JIT_IR_SPLICE_SWITCH`), jump-table range arithmetic (`key - low`
// computed in 32 bits must not wrap into the table: tables at MIN_VALUE and
// at MAX_VALUE, a table spanning negative and positive keys, `hi - lo`
// near 2^31 forced into a lookupswitch), byte / char / short selectors
// (sign-extension: byte -128, char 0xffff, short -32768), String switches
// with hash collisions ("Aa" / "BB", "AaAa" / "BBBB" / "AaBB" / "BBAa", the
// empty string, "NUL" (one U+0000 char) whose hashCode is 0 but is not "", and
// "polygenelubricants" whose hashCode is Integer.MIN_VALUE, so the hash
// lookupswitch has a MIN_VALUE key), switch on a
// boxed Integer, enum switches, and type-pattern switches with guards over
// a non-sealed interface. Profile pruning is the deopt trigger: every kernel
// is warmed with one or two hot keys (so a speculating tier may prune the
// other arms into uncommon traps), then fed every edge key, then a
// never-loaded implementation reaches the pattern switch, then all again.
// A null selector prints only the exception class (HotSpot's helpful NPE text
// names javac locals).
//
// Run:
//   javac -d out R21Fz4SwitchEdge1.java
//   java -Xmx256m -cp out R21Fz4SwitchEdge1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz4SwitchEdge1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz4SwitchEdge1
//   cratonvm --nojit -Xmx256m -cp out R21Fz4SwitchEdge1
//   CRATONVM_JIT_IR_SWITCH_SEARCH_TREE=1 CRATONVM_JIT_IR_SPLICE_SWITCH=1 cratonvm ...
//   CRATONVM_JIT_IR_SWITCH_SEARCH_TREE=0 cratonvm ...     # control arm
// HotSpot expectation: every line identical on every arm.

public class R21Fz4SwitchEdge1 {
    static final int WARM = 80_000;
    static final int MIN = Integer.MIN_VALUE;
    static final int MAX = Integer.MAX_VALUE;

    static final int[] KEYS = {MIN, MIN + 1, MIN + 2, MIN + 3, MIN + 4, -65536, -129, -128, -2, -1, 0, 1, 2, 3, 4, 5,
        9, 10, 11, 15, 20, 21, 127, 128, 255, 256, 1000, 1 << 20, 65535, 65536, MAX - 4, MAX - 3, MAX - 2, MAX - 1, MAX};

    static int tableLow(int k) {
        switch (k) {
            case MIN: return 1;
            case MIN + 1: return 2;
            case MIN + 2: return 3;
            case MIN + 3: return 4;
            default: return 0;
        }
    }

    static int tableHigh(int k) {
        switch (k) {
            case MAX - 3: return 11;
            case MAX - 2: return 12;
            case MAX - 1: return 13;
            case MAX: return 14;
            default: return 10;
        }
    }

    static int tableMid(int k) {
        switch (k) {
            case -2: return 21;
            case -1: return 22;
            case 0: return 23;
            case 1: return 24;
            case 2: return 25;
            case 3: return 26;
            case 4: return 27;
            case 5: return 28;
            case 9: return 29;
            case 10: return 30;
            default: return 20;
        }
    }

    static int sparse(int k) {
        switch (k) {
            case MIN: return 31;
            case -65536: return 32;
            case -1: return 33;
            case 0: return 34;
            case 21: return 35;
            case 1000: return 36;
            case 1 << 20: return 37;
            case MAX: return 38;
            default: return 30;
        }
    }

    // Dense run plus far outliers on both ends: one table plus compares.
    static int mixed(int k) {
        return switch (k) {
            case MIN -> 41;
            case 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20 -> 42 + (k & 1);
            case 128 -> 44;
            case MAX -> 45;
            default -> {
                int r = 40;
                if (k < 0) {
                    r--;
                }
                yield r;
            }
        };
    }

    static int bytes(byte b) {
        switch (b) {
            case -128: return 51;
            case -1: return 52;
            case 0: return 53;
            case 127: return 54;
            default: return 50;
        }
    }

    static int chars(char c) {
        switch (c) {
            case 0: return 61;
            case 0x7f: return 62;
            case 0x80: return 63;
            case 0xfffe: return 64;
            case 0xffff: return 65;
            default: return 60;
        }
    }

    static int shorts(short s) {
        switch (s) {
            case -32768: return 71;
            case -1: return 72;
            case 32767: return 73;
            default: return 70;
        }
    }

    static int strings(String s) {
        switch (s) {
            case "Aa": return 81;
            case "BB": return 82;
            case "AaAa": return 83;
            case "BBBB": return 84;
            case "AaBB": return 85;
            case "BBAa": return 86;
            case "": return 87;
            case "\u00e9": return 88;
            case "\u20ac": return 89;
            case "polygenelubricants": return 78;
            case "\u0000": return 77;
            default: return 80;
        }
    }

    static int boxed(Integer k) {
        switch (k) {
            case 0: return 91;
            case 127: return 92;
            case 128: return 93;
            case -129: return 94;
            default: return 90;
        }
    }

    enum Color { RED, GREEN, BLUE, ALPHA }

    static int colors(Color c) {
        switch (c) {
            case RED: return 101;
            case BLUE: return 102;
            default: return 100;
        }
    }

    static int colorsArrow(Color c) {
        return switch (c) {
            case RED -> 111;
            case GREEN -> 112;
            case BLUE -> 113;
            case ALPHA -> 114;
        };
    }

    interface Shape {
        int size();
    }

    record Sq(int side) implements Shape {
        public int size() {
            return side * side;
        }
    }

    record Circ(int r) implements Shape {
        public int size() {
            return 3 * r * r;
        }
    }

    // Never loaded before the `late` pass.
    record Tri(int b, int h) implements Shape {
        public int size() {
            return b * h / 2;
        }
    }

    static int shapes(Object o) {
        return switch (o) {
            case null -> 120;
            case Sq s when s.side() > 100 -> 121;
            case Sq s -> 122 + (s.side() & 1);
            case Circ c when c.r() == 0 -> 124;
            case Circ c -> 125;
            case Shape sh -> 126 + (sh.size() & 1);
            case Integer i when i > 0 -> 128;
            case Integer i -> 129;
            case String s -> 130 + s.length();
            default -> 119;
        };
    }

    static String row(int k) {
        return tableLow(k) + "," + tableHigh(k) + "," + tableMid(k) + "," + sparse(k) + "," + mixed(k) + ","
            + bytes((byte) k) + "," + chars((char) k) + "," + shorts((short) k) + "," + boxed(k);
    }

    static final String[] STRS = {"Aa", "BB", "AaAa", "BBBB", "AaBB", "BBAa", "BBBBBB", "", "a", "A",
        "\u00e9", "\u20ac", "polygenelubricants", "Ab", "aa", "\ud83d\ude00", "\u0000", "\u0000\u0000"};

    static Object[] shapeInputs(boolean late) {
        return new Object[] {null, new Sq(3), new Sq(4), new Sq(101), new Circ(0), new Circ(2), Integer.valueOf(5),
            Integer.valueOf(-5), "xyz", "", 3.5, late ? new Tri(3, 5) : "late", new int[0]};
    }

    static void facts(String pass, boolean late) {
        StringBuilder sb = new StringBuilder();
        for (int k : KEYS) {
            System.out.println(pass + " key " + k + " " + row(k));
        }
        for (String s : STRS) {
            sb.append(' ').append(strings(s));
        }
        System.out.println(pass + " strings" + sb + " hashMin=" + "polygenelubricants".hashCode());
        try {
            strings(null);
            System.out.println(pass + " strings.null none");
        } catch (NullPointerException e) {
            System.out.println(pass + " strings.null " + e.getClass().getName());
        }
        try {
            boxed(null);
            System.out.println(pass + " boxed.null none");
        } catch (NullPointerException e) {
            System.out.println(pass + " boxed.null " + e.getClass().getName());
        }
        sb.setLength(0);
        for (Color c : Color.values()) {
            sb.append(' ').append(colors(c)).append('/').append(colorsArrow(c));
        }
        System.out.println(pass + " colors" + sb);
        try {
            colors(null);
            System.out.println(pass + " colors.null none");
        } catch (NullPointerException e) {
            System.out.println(pass + " colors.null " + e.getClass().getName());
        }
        sb.setLength(0);
        for (Object o : shapeInputs(late)) {
            sb.append(' ').append(shapes(o));
        }
        System.out.println(pass + " shapes" + sb);
    }

    // Narrow profile: key 3 / 4 only, "Aa" only, RED only, Sq only.
    static long warmNarrow() {
        long h = 0;
        Sq sq = new Sq(7);
        for (int i = 0; i < WARM; i++) {
            int k = 3 + (i & 1);
            h = h * 31 + tableLow(k) + tableHigh(k) + tableMid(k) + sparse(k) + mixed(k)
                + bytes((byte) k) + chars((char) k) + shorts((short) k) + boxed(k);
            h = h * 31 + strings("Aa") + colors(Color.RED) + colorsArrow(Color.RED) + shapes(sq);
        }
        return h;
    }

    // Wide profile: every key, string and shape.
    static long warmWide(boolean late) {
        long h = 0;
        Object[] shapes = shapeInputs(late);
        Color[] cs = Color.values();
        for (int i = 0; i < WARM; i++) {
            int k = KEYS[i % KEYS.length];
            h = h * 31 + tableLow(k) + tableHigh(k) + tableMid(k) + sparse(k) + mixed(k)
                + bytes((byte) k) + chars((char) k) + shorts((short) k) + boxed(k);
            h = h * 31 + strings(STRS[i % STRS.length]) + colors(cs[i & 3]) + colorsArrow(cs[i & 3])
                + shapes(shapes[i % shapes.length]);
        }
        return h;
    }

    public static void main(String[] args) {
        facts("cold", false);
        System.out.println("narrow h=" + Long.toHexString(warmNarrow()));
        facts("edge", false);
        System.out.println("wide h=" + Long.toHexString(warmWide(false)));
        facts("late", true);
        System.out.println("lateWide h=" + Long.toHexString(warmWide(true)));
        System.out.println("narrow2 h=" + Long.toHexString(warmNarrow()));
        facts("final", true);
        System.out.println("done");
    }
}
