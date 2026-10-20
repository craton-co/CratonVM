// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: STRINGS21-1, String-internal accessors at
// the planning gate (`CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT`: the IR prefix
// kinds `StrCoder` / `StrIsLatin1` and the admitted `String.<init>([BB)V`),
// which move JDK String code (`CaseInsensitiveComparator.compare`,
// `StringLatin1.toLowerCase` / `toUpperCase`, `String.concat`, `repeat`,
// `strip`, `indent`, `String.join`, `replace`) to the optimizing tier.
//
// Risk targeted: `coder()` must answer `UTF16` when COMPACT_STRINGS is off
// and the field otherwise; `isLatin1()` likewise; `new String(bytes, coder)`
// as an ordinary bound call (it stores `value` / `coder`, no hash). A wrong
// coder would read a LATIN1 array as UTF16 (half the length, garbage chars)
// or the reverse. Every JDK routine here branches on the coder; inputs mix
// LATIN1 and UTF16 operands in both orders, plus strings that were built as
// UTF16 and compacted. Output: the strings themselves (as code-point hex),
// lengths, hashes, comparator signs, sorted orders.
//
// Run:
//   javac -d out R21FzStrCoder1.java
//   java -Xmx256m -cp out R21FzStrCoder1                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzStrCoder1
//   CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT=1 cratonvm -Xmx256m -cp out R21FzStrCoder1
//   CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT=1 CRATONVM_JIT_IR_STRING_COMPARE_TO_CI_PREFIX=1 CRATONVM_JIT_IR_STRING_LOWER_UNCHANGED_PREFIX=1 cratonvm ...
//   java -XX:-CompactStrings ... / cratonvm -XX:-CompactStrings ... (if accepted: the flag-byte arm)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.Arrays;
import java.util.Locale;

public class R21FzStrCoder1 {
    static final int WARM = 30_000;

    static final String[] IN = {
        "", "abc", "ABC", "MiXeD cAsE 123", "été ÉTÉ", "µÿß",
        "Āā", "Straße", "中文abc", "  pad  ", "\t wide space ",
        "😀smile", new String(new char[] {'l', 'a', 't', 'i', 'n'}), "x".repeat(17),
    };

    static String hex(String s) {
        StringBuilder sb = new StringBuilder();
        s.codePoints().forEach(c -> sb.append(Integer.toHexString(c)).append('.'));
        return sb.toString();
    }

    static long digest(String s) {
        return ((long) s.hashCode() << 16) ^ s.length();
    }

    static long work(String a, String b) {
        long h = 0;
        h = h * 31 + digest(a.toLowerCase(Locale.ROOT));
        h = h * 31 + digest(a.toUpperCase(Locale.ROOT));
        h = h * 31 + digest(a.concat(b));
        h = h * 31 + digest(b.concat(a));
        h = h * 31 + digest(a.repeat(2));
        h = h * 31 + digest(a.strip());
        h = h * 31 + digest(String.join("|", a, b));
        h = h * 31 + digest(a.replace('a', '中'));
        h = h * 31 + digest(b.replace("c", ""));
        h = h * 31 + Integer.signum(String.CASE_INSENSITIVE_ORDER.compare(a, b));
        h = h * 31 + (a.equalsIgnoreCase(b) ? 1 : 0);
        h = h * 31 + (a.regionMatches(true, 0, b, 0, Math.min(a.length(), b.length())) ? 1 : 0);
        h = h * 31 + a.indexOf(b);
        h = h * 31 + (a.contentEquals(new StringBuilder(b)) ? 1 : 0);
        return h;
    }

    static void rows(String tag) {
        for (int i = 0; i < IN.length; i++) {
            String a = IN[i];
            System.out.println("row " + tag + " s" + i + " lower=" + hex(a.toLowerCase(Locale.ROOT)) + " upper="
                    + hex(a.toUpperCase(Locale.ROOT)) + " strip=" + hex(a.strip()) + " indent=" + hex(a.indent(2)));
        }
        StringBuilder sb = new StringBuilder("row " + tag + " ci");
        for (String a : IN) {
            for (String b : IN) {
                sb.append(' ').append(Integer.signum(String.CASE_INSENSITIVE_ORDER.compare(a, b)));
            }
        }
        System.out.println(sb);
        String[] sorted = IN.clone();
        Arrays.sort(sorted, String.CASE_INSENSITIVE_ORDER);
        StringBuilder so = new StringBuilder("row " + tag + " sorted");
        for (String s : sorted) {
            so.append(' ').append(hex(s));
        }
        System.out.println(so);
        long h = 0;
        for (String a : IN) {
            for (String b : IN) {
                h = h * 1_000_003L + work(a, b);
            }
        }
        System.out.println("row " + tag + " work " + h);
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            String a = IN[r % IN.length];
            String b = IN[(r / IN.length) % IN.length];
            h = h * 31 + work(a, b);
        }
        System.out.println("warm hash " + h);
        rows("hot");
        // A rare branch: a Locale whose language takes StringLatin1's slow path.
        Locale tr = Locale.forLanguageTag("tr");
        System.out.println("tr " + hex("TITLE I".toLowerCase(tr)) + " " + hex("title i".toUpperCase(tr)));
        rows("after");
    }
}
