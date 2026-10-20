// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// S17Empty: identity of the empty String across literal sites and JDK methods.
public class S17Empty {
    static String lit() { return ""; }
    public static void main(String[] a) {
        String e = "";
        System.out.println("lit==lit " + (e == lit()));
        System.out.println("intern " + (e == new String("").intern()));
        System.out.println("sb0 " + (new StringBuilder().toString() == e));
        System.out.println("sb16 " + (new StringBuilder(16).toString() == e));
        StringBuilder sb = new StringBuilder("x"); sb.setLength(0);
        System.out.println("sbSetLen " + (sb.toString() == e));
        System.out.println("substring " + ("abc".substring(1, 1) == e));
        System.out.println("valueOfEmptyChars " + (String.valueOf(new char[0]) == e));
        System.out.println("concat " + ("".concat("") == e));
        System.out.println("strip " + ("  ".strip() == e));
        System.out.println("repeat " + ("a".repeat(0) == e));
        System.out.println("join " + (String.join(",", new String[0]) == e));
        System.out.println("sbf0 " + (new StringBuffer().toString() == e));
        int same = 0;
        for (int i = 0; i < 100000; i++) if (new StringBuilder().toString() == e) same++;
        System.out.println("loop " + same);
    }
}
