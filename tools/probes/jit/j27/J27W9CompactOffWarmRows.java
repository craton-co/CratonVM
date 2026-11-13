// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 9, lane W9VER: `-XX:-CompactStrings` through WARM (compiled) String producers and
// readers (page docs/feature-designs/real-jdk-mode.md). Every
// row runs its body WARM times so both tiers compile it, then prints one fact from the last
// result: the text (or a checksum) and, with --add-opens, the result's coder.
//
//   javac -d <dir> J27W9CompactOffWarmRows.java
//   java     --add-opens java.base/java.lang=ALL-UNNAMED [-XX:-CompactStrings] -cp <dir> J27W9CompactOffWarmRows
//   cratonvm --add-opens java.base/java.lang=ALL-UNNAMED [-XX:-CompactStrings] --cp <dir> J27W9CompactOffWarmRows
//     (--real-jdk, --compatible, --nojit)
// Default arm: every Latin-1 result has coder 0; `-XX:-CompactStrings`: every coder is 1. The
// text / checksum part of each line is the same in both arms (take the HotSpot run as reference).

import java.lang.reflect.Field;

public class J27W9CompactOffWarmRows {
    static final int WARM = 30_000;
    static Field coderField;
    static volatile int sink;

    static String coder(String s) {
        try {
            if (coderField == null) {
                coderField = String.class.getDeclaredField("coder");
                coderField.setAccessible(true);
            }
            return String.valueOf(coderField.getByte(s));
        } catch (ReflectiveOperationException | RuntimeException e) {
            return "?";
        }
    }

    static void fact(String name, String s) {
        System.out.println("fact " + name + " " + s + " coder=" + coder(s) + " hash=" + s.hashCode());
    }

    interface Row { String run(int i); }

    static void row(String name, Row r) {
        String last = null;
        int acc = 0;
        for (int i = 0; i < WARM; i++) {
            last = r.run(i);
            acc += last.length() + last.charAt(last.length() - 1);
        }
        sink = acc;
        fact(name, last);
        System.out.println("fact " + name + "-acc " + acc);
    }

    static String concat(int i) { return "k" + i + "-" + (char) ('a' + (i % 26)); }
    static String sb(int i) { return new StringBuilder().append("v").append(i).append('!').toString(); }
    static String valueOfChars(int i) { char[] c = {'x', (char) ('0' + i % 10), 'z'}; return String.valueOf(c); }
    static String newChars(int i) { char[] c = {'n', (char) ('0' + i % 10)}; return new String(c); }
    static String intToString(int i) { return Integer.toString(i * 7); }
    static String lower(int i) { return ("MiXeD" + (i % 10)).toLowerCase(); }
    static String upper(int i) { return ("mixed" + (i % 10)).toUpperCase(); }
    static String sub(int i) { return ("prefix-" + i).substring(3, 7); }
    static String bytes(int i) { byte[] b = {'b', 'y', (byte) ('0' + i % 10)}; return new String(b, java.nio.charset.StandardCharsets.ISO_8859_1); }
    static String euro(int i) { return "€" + (i % 10); }
    static String join(int i) { return String.join(",", "a", Integer.toString(i % 100)); }
    static String repeat(int i) { return "ab".repeat(1 + i % 3); }
    static String strip(int i) { return ("  s" + (i % 10) + "  ").strip(); }
    static String replace(int i) { return ("r-" + (i % 10)).replace('-', '+'); }
    static String format(int i) { return String.valueOf(i % 1000) + String.valueOf((long) i) + String.valueOf(i % 2 == 0); }

    public static void main(String[] args) {
        row("concat", J27W9CompactOffWarmRows::concat);
        row("sb", J27W9CompactOffWarmRows::sb);
        row("valueof-chars", J27W9CompactOffWarmRows::valueOfChars);
        row("new-chars", J27W9CompactOffWarmRows::newChars);
        row("int-tostring", J27W9CompactOffWarmRows::intToString);
        row("lower", J27W9CompactOffWarmRows::lower);
        row("upper", J27W9CompactOffWarmRows::upper);
        row("substring", J27W9CompactOffWarmRows::sub);
        row("bytes-latin1", J27W9CompactOffWarmRows::bytes);
        row("euro", J27W9CompactOffWarmRows::euro);
        row("join", J27W9CompactOffWarmRows::join);
        row("repeat", J27W9CompactOffWarmRows::repeat);
        row("strip", J27W9CompactOffWarmRows::strip);
        row("replace", J27W9CompactOffWarmRows::replace);
        row("valueof-prims", J27W9CompactOffWarmRows::format);
        int eq = 0, idx = 0;
        for (int i = 0; i < WARM; i++) {
            String a = concat(i), b = "k" + i + "-" + (char) ('a' + (i % 26));
            if (a.equals(b)) eq++;
            idx += a.indexOf('-') + a.compareTo(b) + (a.contentEquals(b) ? 1 : 0);
        }
        System.out.println("fact readers " + eq + " " + idx);
        String lit = "literal";
        System.out.println("fact literal " + lit + " coder=" + coder(lit) + " intern=" + (lit == ("lit" + "eral").intern()));
        System.out.println("fact done");
    }
}
