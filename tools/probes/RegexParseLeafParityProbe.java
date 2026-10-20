// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/** Cold and compiled Long.parseLong successes, declines and exact error stacks. */
public class RegexParseLeafParityProbe {
    static String parse(String text, int radix, boolean decimal) {
        try {
            return "value=" + (decimal ? Long.parseLong(text) : Long.parseLong(text, radix));
        } catch (NumberFormatException e) {
            StringBuilder out = new StringBuilder(e.getClass().getName()).append(':').append(e.getMessage());
            for (StackTraceElement frame : e.getStackTrace()) {
                out.append(" | ").append(frame);
                if (frame.getClassName().equals(RegexParseLeafParityProbe.class.getName())) break;
            }
            return out.toString();
        }
    }

    static long warm(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += Long.parseLong("123456789");
            sum += Long.parseLong("7fffffff", 16);
        }
        return sum;
    }

    static void rows(String phase) {
        String[] text = {
            null, "", "0", "-0", "+42", "-42", "9223372036854775807", "-9223372036854775808",
            "9223372036854775808", "-9223372036854775809", "+", "--1", " 1", "1 ", "1_0", "1L",
            "\uff11\uff12", "\u0661\u0662", "\ud835\udfce", "\ud800", "\udc00",
            "0".repeat(100) + "123", "-" + "0".repeat(100) + "1"
        };
        for (int i = 0; i < text.length; i++) {
            System.out.println(phase + " decimal " + i + " " + parse(text[i], 10, true));
            for (int radix : new int[] {1, 2, 10, 16, 36, 37}) {
                System.out.println(phase + " radix " + i + "/" + radix + " " + parse(text[i], radix, false));
            }
        }
    }

    public static void main(String[] args) {
        rows("cold");
        long sum = warm(20_000);
        if (sum != 20_000L * (123456789L + 2147483647L)) throw new AssertionError("warm checksum");
        System.out.println("warm-checksum=" + sum);
        rows("warm");
    }
}
