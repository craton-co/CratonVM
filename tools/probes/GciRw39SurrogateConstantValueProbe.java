// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gci i39/rw -- a static final String whose {@code ConstantValue} carries a
 * lone surrogate is the interned literal (page
 * {@code docs/known-issues/gc/gci-i39rw-lone-surrogate-constantvalue-string-is-not-the-literal-20261016.md}).
 *
 * <p>HotSpot 25 seeds a {@code ConstantValue} String field with
 * {@code StringTable::intern}, so the field, an {@code ldc} of the same text and
 * {@code intern()} are one object, representable or not. The fields are read
 * by reflection (javac inlines {@code getstatic} of a constant). stdout is
 * deterministic: every comparison line is {@code true} on HotSpot.
 *
 * <pre>
 *   java GciRw39SurrogateConstantValueProbe
 * </pre>
 */
public class GciRw39SurrogateConstantValueProbe {
    static final String CV = "\uD800cv39";
    static final String PLAIN = "plaincv39";

    /** An {@code ldc} of the wide text. */
    static String wideLiteral() {
        return "\uD800cv39";
    }

    /** An {@code ldc} of the representable text. */
    static String plainLiteral() {
        return "plaincv39";
    }

    public static void main(String[] args) throws Exception {
        Object plain = GciRw39SurrogateConstantValueProbe.class.getDeclaredField("PLAIN").get(null);
        Object wide = GciRw39SurrogateConstantValueProbe.class.getDeclaredField("CV").get(null);
        String plainLit = plainLiteral();
        String wideLit = wideLiteral();
        System.out.println("plain field==literal " + (plain == plainLit));
        System.out.println("plain field==intern " + (plain == ((String) plain).intern()));
        System.out.println("wide equals " + wide.equals(wideLit));
        System.out.println("wide literal==intern " + (wideLit == wideLit.intern()));
        System.out.println("wide field==literal " + (wide == wideLit));
        System.out.println("wide field==intern " + (wide == ((String) wide).intern()));
        System.out.println("done");
    }
}
