// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * nio21 wave 9, lane L5: `java.lang.String` and `Character` answers on supplementary characters
 * (surrogate PAIRS) and on UNPAIRED surrogates, for every String method the VM registers as a
 * native (`Intrinsic`) -- the per-code-unit vs per-code-point and lossless vs lossy (U+FFFD) bug
 * classes of docs/internal/fixed-bugs/r21-string-ci-supplementary-pairs-FIXED-20261009.md.
 *
 * Conformance only: every line is a fact. Each row prints its answers cold, then the same rows run
 * in a warm loop (so the call sites compile) and `<row> warm-same true` must follow. Every
 * non-ASCII char is printed as a backslash-u escape.
 *
 *   javac -encoding UTF-8 -d out tools/probes/Nio21StringSupp.java
 *   java -cp out Nio21StringSupp > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21StringSupp > jo.txt
 *   cratonvm --compatible -cp out Nio21StringSupp > co.txt
 *
 * hs, jo and co must be identical. -Dnio21.rows=a,b selects rows
 * (search, compare, build, cps, casemap, bytes, regex, casetable).
 */
public class Nio21StringSupp {
    static final String ROWS = System.getProperty("nio21.rows", "all");
    static final int WARM = 3_000;

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    static String esc(Object o) {
        String s = String.valueOf(o);
        StringBuilder sb = new StringBuilder(s.length() + 16);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x80) sb.append(c);
            else sb.append(String.format("\\u%04x", (int) c));
        }
        return sb.toString();
    }

    static void say(String s) {
        System.out.println(esc(s));
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    // Deseret capital/small, Adlam, an emoji, lone high, lone low.
    static final String D = "𐐀";
    static final String d = "𐐨";
    static final String AD = "𞤀";
    static final String EM = "😀";
    static final String HI = "\ud801";
    static final String LO = "\udc00";

    static final String[] S = {
        "a" + D + "b" + D, D + d + D, "x" + HI + "y" + LO + "z", LO + HI, HI + LO, AD + "q" + EM,
        "é" + D, "abc", "", D, HI, LO
    };

    interface Row {
        void run(List<String> out);
    }

    static void row(String name, Row r) {
        if (!on(name)) return;
        List<String> cold = new ArrayList<>();
        try {
            r.run(cold);
        } catch (Throwable t) {
            cold.add("THREW " + ex(t));
        }
        for (String s : cold) say(name + " " + s);
        boolean same = true;
        for (int i = 0; i < WARM && same; i++) {
            List<String> warm = new ArrayList<>();
            try {
                r.run(warm);
            } catch (Throwable t) {
                warm.add("THREW " + ex(t));
            }
            same = warm.equals(cold);
            if (!same) for (String s : warm) say(name + " warm " + s);
        }
        say(name + " warm-same " + same);
    }

    static String cps(String s) {
        StringBuilder sb = new StringBuilder();
        s.codePoints().forEach(c -> sb.append(Integer.toHexString(c)).append('.'));
        return sb.toString();
    }

    static String units(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) sb.append(Integer.toHexString(s.charAt(i))).append('.');
        return sb.toString();
    }

    /** indexOf / lastIndexOf / contains / startsWith / endsWith over pairs and lone halves. */
    static void search(List<String> out) {
        int[] needles = {0x10400, 0x10428, 0xD801, 0xDC00, 'b', 0x1F600, 0x110000, -1};
        for (String s : S) {
            StringBuilder sb = new StringBuilder(units(s)).append(':');
            for (int n : needles) {
                sb.append(' ').append(s.indexOf(n)).append('/').append(s.lastIndexOf(n))
                        .append('/').append(s.indexOf(n, 1)).append('/').append(s.lastIndexOf(n, 2));
            }
            for (String t : new String[] {D, HI, LO, "b", "", d}) {
                sb.append(' ').append(s.indexOf(t)).append('/').append(s.lastIndexOf(t)).append('/')
                        .append(s.indexOf(t, 2)).append('/').append(s.lastIndexOf(t, 1)).append('/')
                        .append(s.contains(t) ? 'T' : 'f').append(s.startsWith(t) ? 'T' : 'f')
                        .append(s.endsWith(t) ? 'T' : 'f').append(s.startsWith(t, 1) ? 'T' : 'f');
            }
            out.add(sb.toString());
        }
    }

    /** equals / compareTo / hashCode / contentEquals / regionMatches(false, ..). */
    static void compare(List<String> out) {
        for (String s : S) {
            StringBuilder sb = new StringBuilder(units(s)).append(": ").append(s.hashCode());
            for (String t : S) {
                sb.append(' ').append(s.compareTo(t)).append(s.equals(new String(t)) ? 'T' : 'f')
                        .append(s.contentEquals(new StringBuilder(t)) ? 'T' : 'f')
                        .append(s.regionMatches(1, t, 1, 1) ? 'T' : 'f');
            }
            out.add(sb.toString());
        }
    }

    /** concat / join / replace / repeat / substring / toCharArray / valueOf: must lose nothing. */
    static void build(List<String> out) {
        out.add("concat " + units(HI.concat(LO)) + " " + units(LO.concat(HI)) + " " + units("a".concat(HI))
                + " " + units(HI.concat(LO)).equals(units(D)) + " " + HI.concat(LO).equals(D));
        out.add("join " + units(String.join(HI, "x", LO, "y")) + " "
                + units(String.join(",", Arrays.asList(HI, LO, D))) + " "
                + units(String.join(LO, new StringBuilder(HI), HI)));
        out.add("replace " + units(D.replace('\udc00', '\udc28')) + " " + units(S[2].replace('y', '\udc00'))
                + " " + units(S[0].replace(D, d)) + " " + units(S[0].replace(LO, "!")));
        out.add("repeat " + units(HI.repeat(3)) + " " + units(LO.concat(HI).repeat(2)));
        out.add("substring " + units(S[1].substring(1)) + " " + units(S[1].substring(1, 3)) + " "
                + units(S[0].substring(2)) + " " + units(S[2].subSequence(1, 4).toString()));
        out.add("chars " + units(new String(S[2].toCharArray())) + " " + units(String.valueOf(LO.toCharArray()))
                + " " + units(String.copyValueOf(S[3].toCharArray(), 1, 1)));
        char[] dst = new char[6];
        S[2].getChars(0, 5, dst, 1);
        out.add("getChars " + units(new String(dst)));
        out.add("strip " + units((" " + HI + " ").strip()) + " " + units((" " + LO).trim()) + " "
                + (" " + LO).isBlank() + " " + units((LO + " ").stripTrailing()));
        out.add("intern " + (new String(HI).intern() == HI.intern()) + " " + (new String(LO).intern() == HI.intern())
                + " " + (HI.concat(LO).intern() == D));
        out.add("lines " + HI.concat("\n").concat(LO).lines().map(Nio21StringSupp::units).toList());
        out.add("indent " + units((HI + "\n" + LO).indent(1)));
        out.add("format " + units(String.format("%s|%5s|%-3s", HI, LO, D)) + " " + units("%s".formatted(LO)));
        out.add("cpctor " + units(new String(new int[] {0x10400, 0xD801, 0x41, 0xDC00}, 0, 4)));
    }

    /** codePointAt / Before / Count / offsetByCodePoints / codePoints / chars. */
    static void cps(List<String> out) {
        for (String s : S) {
            StringBuilder sb = new StringBuilder(units(s)).append(": ").append(s.codePointCount(0, s.length()))
                    .append(' ').append(cps(s)).append(' ').append(s.chars().sum());
            for (int i = 0; i < s.length(); i++) {
                sb.append(' ').append(Integer.toHexString(s.codePointAt(i)));
                if (i > 0) sb.append('<').append(Integer.toHexString(s.codePointBefore(i)));
            }
            for (int i = 0; i <= s.length(); i++) {
                try {
                    sb.append(" o").append(s.offsetByCodePoints(i, 1));
                } catch (IndexOutOfBoundsException e) {
                    sb.append(" oX");
                }
            }
            out.add(sb.toString());
        }
    }

    /** toUpperCase / toLowerCase (root, tr, el) of supplementary and lone surrogates. */
    static void casemap(List<String> out) {
        String[] in = {D, d, AD, "𞤢", "𐲀", "𖹀", HI + "a", "A" + LO,
            "ß" + D, "I" + d, LO + HI + "x"};
        for (String s : in) {
            out.add(units(s) + ": " + units(s.toUpperCase()) + " " + units(s.toLowerCase()) + " "
                    + units(s.toUpperCase(java.util.Locale.ROOT)) + " "
                    + units(s.toLowerCase(java.util.Locale.forLanguageTag("tr"))) + " "
                    + units(s.toUpperCase(java.util.Locale.forLanguageTag("el"))));
        }
    }

    /** getBytes / new String(bytes) of lone surrogates (the replacement byte is the JDK's). */
    static void bytes(List<String> out) {
        for (String s : new String[] {HI, LO, S[2], D, LO + HI}) {
            out.add(units(s) + ": " + Arrays.toString(s.getBytes(StandardCharsets.UTF_8)) + " "
                    + Arrays.toString(s.getBytes(StandardCharsets.UTF_16BE)) + " "
                    + Arrays.toString(s.getBytes(StandardCharsets.ISO_8859_1)) + " "
                    + Arrays.toString(s.getBytes(StandardCharsets.US_ASCII)) + " "
                    + units(new String(s.getBytes(StandardCharsets.UTF_16LE), StandardCharsets.UTF_16LE)));
        }
        out.add("decode " + units(new String(new byte[] {(byte) 0xED, (byte) 0xA0, (byte) 0x81, 0x41},
                StandardCharsets.UTF_8)) + " " + units(new String(new byte[] {(byte) 0xF0, (byte) 0x90,
                (byte) 0x90, (byte) 0x80}, StandardCharsets.UTF_8)));
    }

    /** split / matches / replaceAll / replaceFirst with supplementary and lone-surrogate text. */
    static void regex(List<String> out) {
        out.add("split " + Arrays.stream(S[0].split(D)).map(Nio21StringSupp::units).toList() + " "
                + Arrays.stream(S[2].split(HI)).map(Nio21StringSupp::units).toList() + " "
                + Arrays.stream(S[2].split(LO, -1)).map(Nio21StringSupp::units).toList() + " "
                + Arrays.stream(S[1].split("", -1)).map(Nio21StringSupp::units).toList());
        out.add("matches " + D.matches(".") + " " + HI.matches(".") + " " + S[3].matches("..") + " "
                + S[2].matches("x.y.z") + " " + D.matches("[" + D + "-" + d + "]") + " " + d.matches("(?i)" + D));
        out.add("replaceAll " + units(S[0].replaceAll(".", "-")) + " " + units(S[2].replaceAll(LO, "!")) + " "
                + units(S[2].replaceFirst(".", HI)) + " " + units(S[1].replaceAll("(?i)" + d, "_")));
        // nio21 w10 L4: ASCII rows (the natives' exact path) and the shapes it declines.
        out.add("ascii-split " + sp("a,b,,c,,", ",", 0) + sp("a,b,,c,,", ",", -1) + sp("a,b,,c,,", ",", 2)
                + sp("abc", "", 0) + sp(",a", ",", 0) + sp("a1b22c", "\\d+", 0) + sp("aXbXXc", "X*", 0)
                + sp(",", ",", 0) + sp("", ",", 0) + sp("boo:and:foo", "o", -1) + sp("a b\tc", "\\s", 0)
                + sp("ab", "(?:)", 1) + sp("abc", "b|", 0) + sp("a\r\nb", "$", 0) + sp("ab", "(?=b)", 0)
                + sp("aBc", "(?i)b", 0) + sp("x--y", "-+", 0));
        String id = "abc";
        out.add("ascii-identity " + (id.split(",")[0] == id) + " " + (id.replaceAll("x", "y") == id) + " "
                + (id.replaceFirst("x", "y") == id) + " " + ("a,".split(",")[0] == "a") + " "
                + (",b".split(",")[0] == ""));
        out.add("ascii-replace " + "baaa".replaceAll("a*", "-") + "|" + "abc".replaceAll("(b)", "[$1]") + "|"
                + "abc".replaceAll("b", "\\$") + "|" + "x1y22".replaceAll("(\\d)(\\d)?", "<$2$1>") + "|"
                + "aaa".replaceFirst("a", "b") + "|" + "ab12".replaceAll("$", "!") + "|" + "a.b".replaceAll(".", "x")
                + "|" + "abc".replaceAll("x*", "-") + "|" + "ab".replaceAll("(a)(b)", "$21$10") + "|"
                + "aaa".replaceAll("(?<g>a)", "${g}!") + "|" + "abab".replaceAll("(a)(?=b)", "<$1>") + "|"
                + "aab".replaceAll("a++b", "X") + "|" + "a\nb\n".replaceAll("$", "!") + "|" + "x".replaceAll("((x)*)*", "[$2]"));
        out.add("ascii-matches " + "abc".matches("a.c") + " " + "a\rc".matches("a.c") + " " + "ABC".matches("[A-C]+")
                + " " + "ab".matches("a|ab") + " " + "a-".matches("[a-]+") + " " + "x".matches("[^a-c]") + " "
                + "a\n".matches("a$") + " " + "abab".matches("(ab)\\1") + " " + "Ab".matches("(?i)ab") + " "
                + "<a>".matches("\\<a\\>") + " " + "aaa".matches("a{2,3}") + " " + "a".matches("\\p{Lower}"));
        for (String[] bad : new String[][] {{"a", "["}, {"a", "a{2,1}"}, {"a", "*"}}) {
            try {
                out.add("ascii-bad " + bad[0].matches(bad[1]));
            } catch (Throwable t) {
                out.add("ascii-bad " + t.getClass().getName() + " " + t.getMessage().replace("\n", "|"));
            }
        }
        for (String r : new String[] {"\\", "$", "$x", "$2", "${g}"}) {
            try {
                out.add("ascii-badrepl " + "ab".replaceAll("(a)", r) + " " + "zz".replaceAll("(a)", r));
            } catch (Throwable t) {
                out.add("ascii-badrepl " + t.getClass().getName() + " " + t.getMessage());
            }
        }
    }

    static String sp(String s, String r, int l) {
        return Arrays.stream(s.split(r, l)).map(Nio21StringSupp::units).toList().toString();
    }

    /** Character case maps over every code point, folded to hashes (the JDK table is exhaustive). */
    static void casetable(List<String> out) {
        long hu = 0, hl = 0, ht = 0, hcu = 0, hcl = 0;
        int nu = 0, nl = 0, nt = 0;
        for (int c = 0; c <= 0x10FFFF; c++) {
            int u = Character.toUpperCase(c), l = Character.toLowerCase(c), t = Character.toTitleCase(c);
            if (u != c) { hu = hu * 31 + c * 7L + u; nu++; }
            if (l != c) { hl = hl * 31 + c * 7L + l; nl++; }
            if (t != c) { ht = ht * 31 + c * 7L + t; nt++; }
            if (c <= 0xFFFF) {
                char ch = (char) c;
                hcu = hcu * 31 + Character.toUpperCase(ch);
                hcl = hcl * 31 + Character.toLowerCase(ch);
            }
        }
        out.add("int " + nu + " " + hu + " " + nl + " " + hl + " " + nt + " " + ht);
        out.add("char " + hcu + " " + hcl);
        // The per-unit CI fold (LATIN1 / mixed coder) over every BMP pair (c, toLowerCase(c)) and
        // (c, toUpperCase(c)), and a BMP sweep of compareToIgnoreCase against "a".
        long ci = 0;
        int eq = 0;
        for (int c = 0; c <= 0xFFFF; c++) {
            String s = String.valueOf((char) c);
            String l = String.valueOf(Character.toLowerCase((char) c));
            String u = String.valueOf(Character.toUpperCase((char) c));
            if (s.equalsIgnoreCase(l)) eq++;
            if (s.equalsIgnoreCase(u)) eq++;
            if (("x" + s).regionMatches(true, 1, u, 0, 1)) eq++;
            ci = ci * 31 + s.compareToIgnoreCase("a") + 3L * s.compareToIgnoreCase("Ā");
        }
        out.add("ci " + eq + " " + ci);
    }

    public static void main(String[] a) {
        java.util.Locale.setDefault(java.util.Locale.US);
        row("search", Nio21StringSupp::search);
        row("compare", Nio21StringSupp::compare);
        row("build", Nio21StringSupp::build);
        row("cps", Nio21StringSupp::cps);
        row("casemap", Nio21StringSupp::casemap);
        row("bytes", Nio21StringSupp::bytes);
        row("regex", Nio21StringSupp::regex);
        // One warm pass of the full sweep is enough (1.1M code points per pass).
        if (on("casetable")) {
            List<String> c = new ArrayList<>();
            casetable(c);
            for (String s : c) say("casetable " + s);
            List<String> w = new ArrayList<>();
            casetable(w);
            say("casetable warm-same " + w.equals(c));
        }
        say("done");
    }
}
