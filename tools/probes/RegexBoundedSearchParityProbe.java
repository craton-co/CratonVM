// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** Exercise the bounded-search decline after an already-successful native find. */
public class RegexBoundedSearchParityProbe {
    static String dump(Matcher m) {
        StringBuilder out = new StringBuilder();
        int count = 0;
        while (m.find()) {
            if (++count > 2000) throw new AssertionError("nonterminating find");
            for (int group = 0; group <= m.groupCount(); group++) {
                out.append('[').append(m.start(group)).append(',').append(m.end(group))
                    .append(':').append(m.group(group)).append(']');
            }
            out.append('/').append(m.hitEnd()).append('/').append(m.requireEnd()).append(';');
        }
        return out.append("end=").append(m.hitEnd()).append('/').append(m.requireEnd())
            .append(" again=").append(m.find()).toString();
    }

    public static void main(String[] args) {
        Matcher digits = Pattern.compile("(\\d+)").matcher("1 22 333");
        for (int i = 0; i < 3000; i++) dump(digits.reset());
        for (int size : new int[] {0, 1, 254, 255, 256, 257, 511, 512, 513}) {
            System.out.println("gap " + size + " " + dump(digits.reset("1 " + "x".repeat(size) + "22 ")));
            System.out.println("run " + size + " " + dump(digits.reset("1 " + "2".repeat(size) + " 3")));
        }
        for (String pattern : new String[] {"\\d+", "(?:\\d+)", "([a-z]+)", "\\w+", "(\\s+)",
                "([A-Za-z0-9_]+)", "[A-z]+", "a.*b|a", "(?i)[a-z]+", "(\\d+)(\\d+)"}) {
            digits.usePattern(Pattern.compile(pattern));
            System.out.println("pattern " + pattern + " " + dump(digits.reset("1 a_22\t\nZé𐐀 a 333")));
        }
        digits.usePattern(Pattern.compile("(\\d+)"));
        System.out.println("pair " + dump(digits.reset("1 𐐀22 333")));
        System.out.println("pair-region " + dump(digits.reset("1 𐐀22").region(3, 6)));
    }
}
