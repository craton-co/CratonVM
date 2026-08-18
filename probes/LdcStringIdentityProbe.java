// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JVMS §5.1: a string literal is interned. Every `ldc` of the same
// CONSTANT_String entry must push the IDENTICAL reference, and two equal
// literals anywhere in the program must be the same object.
//
// This probe exists because CratonVM had two constructors behind `ldc`:
// the ordinary path pools through `create_java_string`, but the
// surrogate-bearing path (`get_utf8_wide`, taken for literals containing
// lone surrogates -- e.g. ANTLR's `_serializedATN`) called
// `create_java_string_from_units`, which allocates a FRESH object and
// consults no pool. So `==` on a surrogate-bearing literal answered false
// where HotSpot answers true.
//
// Every line prints an exact expected value so the output can be diffed
// against HotSpot rather than eyeballed.
public final class LdcStringIdentityProbe {

    // Lone high surrogate: legal in a Java string literal, not legal UTF-8,
    // so the class file carries it in the wide side table.
    static final String LONE_HIGH = "\uD800";
    static final String LONE_LOW  = "\uDC00";
    // A well-formed surrogate PAIR is ordinary UTF-16 and takes the normal
    // path -- included as the control that must already pass.
    static final String PAIR      = "\uD83D\uDE00";
    static final String PLAIN     = "plain-ascii-literal";

    static String loneHigh() { return "\uD800"; }
    static String pair()     { return "\uD83D\uDE00"; }
    static String plain()    { return "plain-ascii-literal"; }

    public static void main(String[] args) {
        // 1. Same literal, two ldc sites in the same method.
        System.out.println("same-method lone-high: " + ("\uD800" == "\uD800"));
        System.out.println("same-method lone-low:  " + ("\uDC00" == "\uDC00"));
        System.out.println("same-method pair:      " + ("\uD83D\uDE00" == "\uD83D\uDE00"));
        System.out.println("same-method plain:     " + ("plain-ascii-literal" == "plain-ascii-literal"));

        // 2. Literal vs the same literal ldc'd from another method (a
        //    DIFFERENT cp index reached through a different site).
        System.out.println("cross-method lone-high: " + (LONE_HIGH == loneHigh()));
        System.out.println("cross-method pair:      " + (PAIR == pair()));
        System.out.println("cross-method plain:     " + (PLAIN == plain()));

        // 3. Repeated execution of ONE ldc site must be stable.
        String first = null;
        boolean stable = true;
        for (int i = 0; i < 1000; i++) {
            String s = loneHigh();
            if (first == null) first = s;
            else if (first != s) stable = false;
        }
        System.out.println("repeat-stable lone-high: " + stable);

        // 4. intern() must agree with the literal.
        System.out.println("intern lone-high: " + (LONE_HIGH == LONE_HIGH.intern()));
        System.out.println("intern pair:      " + (PAIR == PAIR.intern()));

        // 5. Content must survive whatever the identity fix does.
        System.out.println("len lone-high: " + LONE_HIGH.length()
                + " cp=" + (int) LONE_HIGH.charAt(0));
        System.out.println("len lone-low:  " + LONE_LOW.length()
                + " cp=" + (int) LONE_LOW.charAt(0));
        System.out.println("len pair:      " + PAIR.length()
                + " cp0=" + (int) PAIR.charAt(0) + " cp1=" + (int) PAIR.charAt(1));
        System.out.println("equals lone-high: " + LONE_HIGH.equals(loneHigh()));
    }
}
