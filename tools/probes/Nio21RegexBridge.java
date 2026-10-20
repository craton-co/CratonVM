// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.regex.MatchResult;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.stream.Collectors;

/**
 * nio21 wave 11, lane L2: `java.util.regex.Pattern` / `Matcher` conformance for the VM's regex
 * bridge (`native-builtins/src/regex_matcher.rs`): the real-layout `Matcher.find` / `find(int)` /
 * `start` / `end` / `group` fast path answers only inside the exact subset
 * (`lang_string::java_regex_exact_subset`, switch `CRATONVM_REGEX_BRIDGE_EXACT`) and runs the
 * JDK bytecode otherwise; `Pattern.compile` constructs the JDK's own Pattern.
 *
 * Conformance only: every line is a fact. Each row prints its answers cold, then runs again in a
 * warm loop (so the call sites compile) and `<row> warm-same true` must follow. Every non-ASCII
 * char is printed as a backslash-u escape.
 *
 *   javac -encoding UTF-8 -d out tools/probes/Nio21RegexBridge.java
 *   java -cp out Nio21RegexBridge > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21RegexBridge > jo.txt
 *   cratonvm --compatible -cp out Nio21RegexBridge > co.txt
 *   CRATONVM_REGEX_BRIDGE_EXACT=0 cratonvm --jdk-only -cp out Nio21RegexBridge > jo0.txt
 *
 * hs, jo and co must be identical. Known, reported separately: the `identity` row
 * (`Pattern.compile` cache; identical under `CRATONVM_REGEX_PATTERN_COMPILE_CACHE=0`) and the
 * `hitend` row (the fast path's `hitEnd` / `requireEnd` are approximations). -Dnio21.rows=a,b
 * selects rows (find, region, flags, api, replace, split, hitend, identity).
 */
public class Nio21RegexBridge {
    static final String ROWS = System.getProperty("nio21.rows", "all");
    static final int WARM = 2_000;

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

    interface Row {
        void run(List<String> out) throws Exception;
    }

    static void row(String name, Row r) {
        if (!on(name)) return;
        List<String> cold = new ArrayList<>();
        try {
            r.run(cold);
        } catch (Throwable t) {
            cold.add("THREW " + t.getClass().getName() + ": " + t.getMessage());
        }
        for (String s : cold) say(name + " " + s);
        boolean same = true;
        for (int i = 0; i < WARM && same; i++) {
            List<String> warm = new ArrayList<>();
            try {
                r.run(warm);
            } catch (Throwable t) {
                warm.add("THREW " + t.getClass().getName() + ": " + t.getMessage());
            }
            same = warm.equals(cold);
            if (!same) for (String s : warm) say(name + " warm " + s);
        }
        say(name + " warm-same " + same);
    }

    static final String HI = "\ud801";
    static final String LO = "\udc00";
    static final String D = "𐐀";

    /** Subjects: ASCII, Latin-1, BMP, supplementary pairs, unpaired halves, line terminators. */
    static final String[] TEXT = {
        "abc a1b22c", "café naïve", "аб x١y", "a" + D + "b" + D + D + "c",
        "x" + HI + "y" + LO + "z", "l1\nl2\r\nl3 l4\u0085", "", D
    };

    /** Patterns: in the exact subset, and the shapes it declines. */
    static final String[] PAT = {
        "a", "[a-c]+", "\\d+", "\\w+", "\\W", "\\s", ".", ".+", "[^a]", "\\b", "\\B", "x*", "(a)(b)?",
        "a|ab", "$", "^", "(?:a|b)c?", "[\\d,]+", "(?=b)", "(?i)A", "\\p{L}+", "\\X", "(a)\\1", "a++",
        "(?<n>b)", "\\R", "(?m)^l", "(?s).+"
    };

    static String findAll(Matcher m) {
        StringBuilder sb = new StringBuilder();
        while (m.find()) {
            sb.append(m.start()).append('-').append(m.end());
            for (int g = 1; g <= m.groupCount(); g++) {
                sb.append('/').append(m.start(g)).append(':').append(m.group(g) == null ? "N" : m.group(g).length());
            }
            sb.append(' ');
        }
        return sb.toString().trim();
    }

    static void find(List<String> out) {
        for (String p : PAT) {
            Pattern pat = Pattern.compile(p);
            for (int t = 0; t < TEXT.length; t++) {
                out.add(p + " #" + t + ": " + findAll(pat.matcher(TEXT[t])));
            }
            Matcher m = pat.matcher(TEXT[3]);
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i <= TEXT[3].length(); i++) {
                sb.append(m.find(i) ? m.start() + "-" + m.end() : "f").append(' ');
            }
            out.add(p + " findAt: " + sb.toString().trim());
        }
    }

    static void region(List<String> out) {
        String s = "ab" + D + "cd ab";
        for (String p : new String[] {"ab", "^ab", "ab$", "\\b", ".", "x*"}) {
            Pattern pat = Pattern.compile(p);
            for (int[] r : new int[][] {{0, s.length()}, {1, 6}, {2, 4}, {3, 7}, {5, 9}}) {
                Matcher m = pat.matcher(s).region(r[0], r[1]);
                String a = findAll(m);
                Matcher t = pat.matcher(s).region(r[0], r[1]).useTransparentBounds(true);
                Matcher n = pat.matcher(s).region(r[0], r[1]).useAnchoringBounds(false);
                out.add(p + " [" + r[0] + "," + r[1] + "] " + a + " | t " + findAll(t) + " | n " + findAll(n)
                        + " | " + m.regionStart() + "," + m.regionEnd() + " " + m.hasTransparentBounds()
                        + m.hasAnchoringBounds());
            }
        }
    }

    static void flags(List<String> out) {
        String s = "KkK SSß Iİiı Σσς a\nB";
        int[] fl = {0, Pattern.CASE_INSENSITIVE, Pattern.CASE_INSENSITIVE | Pattern.UNICODE_CASE,
            Pattern.UNICODE_CHARACTER_CLASS, Pattern.MULTILINE, Pattern.DOTALL, Pattern.COMMENTS,
            Pattern.LITERAL, Pattern.UNIX_LINES};
        for (int f : fl) {
            for (String p : new String[] {"k", "s+", "i", "σ", "\\w+", "^b", ".$", "a # c\n.", "a.B"}) {
                try {
                    Pattern pat = Pattern.compile(p, f);
                    out.add(f + " " + p + ": " + findAll(pat.matcher(s)) + " flags=" + pat.flags());
                } catch (Exception e) {
                    out.add(f + " " + p + ": " + e.getClass().getName());
                }
            }
        }
        try {
            Pattern.compile("a", 0x10000);
        } catch (Exception e) {
            out.add("badflag " + e.getClass().getName() + ": " + e.getMessage());
        }
        try {
            Pattern.compile(null);
        } catch (Exception e) {
            out.add("null " + e.getClass().getName());
        }
    }

    static void api(List<String> out) {
        Pattern p = Pattern.compile("(\\d+)([a-z]?)");
        String s = "12a 3 " + D + "45b 6";
        Matcher m = p.matcher(s);
        out.add("matches " + m.matches() + " lookingAt " + m.lookingAt() + " " + m.start() + "-" + m.end()
                + " hasMatch " + m.hasMatch());
        m.reset();
        out.add("results " + m.results().map(r -> r.group() + "@" + r.start(1) + "/" + r.group(2))
                .collect(Collectors.joining(",")));
        m.reset();
        List<MatchResult> rs = new ArrayList<>();
        while (m.find()) rs.add(m.toMatchResult());
        out.add("toMatchResult " + rs.stream().map(r -> r.start() + ":" + r.group(1)).collect(Collectors.joining(",")));
        out.add("quote " + Pattern.quote("a\\E.b") + " " + "a\\E.b".matches(Pattern.quote("a\\E.b"))
                + " " + Matcher.quoteReplacement("$1\\x"));
        out.add("splitAsStream " + p.splitAsStream(s).collect(Collectors.toList()));
        out.add("asPredicate " + p.asPredicate().test("x1") + " " + p.asMatchPredicate().test("x1") + " "
                + p.asMatchPredicate().test("12z"));
        out.add("matches-static " + Pattern.matches("\\d+", "123") + " " + Pattern.matches("\\d+", "12" + D));
        out.add("pattern " + p.pattern() + " " + p + " flags " + p.flags());
        Matcher u = p.matcher("7x 8y");
        u.find();
        u.usePattern(Pattern.compile("[a-z]"));
        out.add("usePattern " + findAll(u));
        Matcher e = p.matcher("1a");
        e.find();
        try {
            e.group(3);
        } catch (Exception x) {
            out.add("group3 " + x.getClass().getName() + ": " + x.getMessage());
        }
        try {
            e.find(-1);
        } catch (Exception x) {
            out.add("find-1 " + x.getClass().getName() + ": " + x.getMessage());
        }
        Matcher n = p.matcher("x");
        try {
            n.start();
        } catch (Exception x) {
            out.add("nomatch " + x.getClass().getName() + ": " + x.getMessage());
        }
    }

    static void replace(List<String> out) {
        Pattern p = Pattern.compile("(\\w)(\\d)?");
        String s = "a1 b " + D + " c3";
        out.add("replaceAll " + p.matcher(s).replaceAll("<$2$1>") + " | " + p.matcher(s).replaceFirst("[$0]")
                + " | " + p.matcher(s).replaceAll(r -> r.group(1).toUpperCase()));
        Matcher m = p.matcher(s);
        StringBuilder sb = new StringBuilder();
        StringBuffer sf = new StringBuffer();
        while (m.find()) m.appendReplacement(sb, "{$1}");
        m.appendTail(sb);
        m.reset();
        while (m.find()) m.appendReplacement(sf, "($1)");
        m.appendTail(sf);
        out.add("append " + sb + " | " + sf);
        out.add("empty " + Pattern.compile("x*").matcher("a" + D + "b").replaceAll("-"));
        out.add("lone " + Pattern.compile(".").matcher(HI + "a" + LO).replaceAll("[$0]"));
    }

    static void split(List<String> out) {
        for (String p : new String[] {",", "", "x*", "\\b", "(?=b)", "\\s*,\\s*", "."}) {
            Pattern pat = Pattern.compile(p);
            for (String s : new String[] {"a,b,,c,,", "a" + D + "b", "", ",a b", HI + "," + LO}) {
                out.add(p + " " + Arrays.toString(pat.split(s)) + Arrays.toString(pat.split(s, -1))
                        + Arrays.toString(pat.split(s, 2)));
            }
        }
    }

    static void hitend(List<String> out) {
        String[][] h = {{"a", "a"}, {"a+", "aa"}, {"ab", "a"}, {"a|ab", "a"}, {"x", "abc"}, {"a*", "b"},
            {"\\d+", "12x"}, {"b", "ab"}, {"[a-c]+", "abc"}, {"a$", "a"}};
        for (String[] p : h) {
            Matcher m = Pattern.compile(p[0]).matcher(p[1]);
            boolean f = m.find();
            out.add(p[0] + " " + p[1] + " " + f + " " + m.hitEnd() + " " + m.requireEnd());
        }
    }

    static void identity(List<String> out) {
        out.add("compile-same " + (Pattern.compile("a") == Pattern.compile("a")) + " "
                + (Pattern.compile("a", 2) == Pattern.compile("a", 2)));
    }

    public static void main(String[] a) {
        row("find", Nio21RegexBridge::find);
        row("region", Nio21RegexBridge::region);
        row("flags", Nio21RegexBridge::flags);
        row("api", Nio21RegexBridge::api);
        row("replace", Nio21RegexBridge::replace);
        row("split", Nio21RegexBridge::split);
        row("hitend", Nio21RegexBridge::hitend);
        row("identity", Nio21RegexBridge::identity);
        say("done");
    }
}
