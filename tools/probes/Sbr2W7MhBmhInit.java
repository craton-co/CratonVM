// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR round 2 wave 7 lane MH: initialise the real {@code BoundMethodHandle} directly
 * (no retired row needed) to measure what the real species machinery needs next.
 * Run twice: plain, and with {@code CRATONVM_MHN_STATIC_FIELD_REAL=1}. The second run
 * is the stage-2 entry of the carrier-replacement design page.
 */
public class Sbr2W7MhBmhInit {
    public static void main(String[] args) throws Throwable {
        for (String n : new String[] {"java.lang.invoke.BoundMethodHandle", "java.lang.invoke.BoundMethodHandle$Species_L",
                "java.lang.invoke.LambdaForm", "java.lang.invoke.DirectMethodHandle", "java.lang.invoke.Invokers"}) {
            try {
                Class<?> c = Class.forName(n, true, null);
                System.out.println("init " + n + " ok");
            } catch (Throwable t) {
                System.out.println("init " + n + " FAILED " + t);
                int d = 0;
                for (Throwable c = t; c != null && d < 5; c = c.getCause(), d++) {
                    System.out.println("  cause[" + d + "] " + c);
                    StackTraceElement[] st = c.getStackTrace();
                    for (int i = 0; i < Math.min(st.length, 10); i++) System.out.println("      at " + st[i]);
                }
            }
        }
    }
}
