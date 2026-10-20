// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * zs s1/nd: an OutOfMemoryError while the launcher builds an INSTANCE main's
 * receiver (JDK 25 JEP 512: {@code java.c} runs {@code new Main()} after the
 * class's {@code <clinit>}). {@code zgc-zr4ab-infallible-residuals-20261012.md}
 * item 7b.
 *
 * <p>The static initializer fills the heap and KEEPS it (a chain of nodes held
 * by a static field), down to object granularity, then returns. The
 * launcher's {@code new Zs1NdLauncherOome()} then has no room. HotSpot prints
 * on stderr {@code Exception in thread "main" java.lang.OutOfMemoryError: Java
 * heap space} and exits 1; {@code main} never runs.
 *
 * <p>stdout: {@code start} (from the initializer, before the fill), and {@code main-ran} only
 * if the receiver could be made (a row where the fill left room: not a
 * verdict, re-run with a smaller {@code -Xmx}). Usage:
 * {@code java -Xmx16m Zs1NdLauncherOome}.
 */
public class Zs1NdLauncherOome {
    static Object[] chain;

    static {
        // Printed BEFORE the fill: on a full heap the print itself could fail.
        System.out.println("start");
        int size = 1 << 20;
        Object[] head = null;
        try {
            while (true) {
                try {
                    Object[] node = new Object[2];
                    node[0] = size > 0 ? new long[size] : new Object();
                    node[1] = head;
                    head = node;
                } catch (OutOfMemoryError e) {
                    if (size == 0) {
                        break;
                    }
                    size >>= 1;
                }
            }
        } finally {
            chain = head;
        }
    }

    void main() {
        System.out.println("main-ran " + (chain != null));
    }
}
