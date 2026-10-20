// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * zr z4/ab positive control for a converted VM-internal allocation: the
 * launcher's instance-{@code main} receiver ({@code java.c}'s
 * {@code new Main()}, JEP 512, final in JDK 25).
 *
 * <p>The static initializer fills the heap to the last object and keeps it
 * all reachable, so the receiver the launcher allocates right after
 * {@code <clinit>} cannot fit even after a collection. HotSpot throws
 * {@code OutOfMemoryError} from the launcher ({@code Exception in thread
 * "main" java.lang.OutOfMemoryError: Java heap space}, rc 1). Before zr z4/ab
 * the receiver came from ZGC's infallible arm, which aborts (rc 134, a
 * {@code FATAL: ZGC(real)} block); with z4/ab it goes through the native
 * collecting ladder and the launcher reports the OOME (rc 1).
 * {@code CRATONVM_ZGC_INTERNAL_ALLOC_OOME=0} restores the abort.
 *
 * <p>The receiver carries eight {@code long} fields so it is larger than the
 * smallest filler object: the last fill step stops only when no filler fits,
 * so no receiver fits either.
 *
 * <p>Run: {@code -XX:+UseZGC -Xmx48m Zr4AbLauncherReceiver}. Pass: rc 1 and
 * {@code java.lang.OutOfMemoryError} on stderr, never rc 134. The stdout is
 * the single line {@code filled} (printed by the initializer, before it fills).
 */
public class Zr4AbLauncherReceiver {
    static final class Node {
        Node next;
    }

    static java.util.ArrayList<Object> keep;
    static Node chain;

    long a0, a1, a2, a3, a4, a5, a6, a7;

    static {
        // Printed before the fill: at the ceiling even println may allocate.
        System.out.println("filled");
        keep = new java.util.ArrayList<>(1 << 16);
        try {
            while (true) {
                keep.add(new byte[64 * 1024]);
            }
        } catch (OutOfMemoryError e) {
            // Big chunks no longer fit.
        }
        try {
            while (true) {
                keep.add(new byte[256]);
            }
        } catch (OutOfMemoryError e) {
            // Small ones no longer fit.
        }
        try {
            while (true) {
                Node n = new Node();
                n.next = chain;
                chain = n;
            }
        } catch (OutOfMemoryError e) {
            // Not even a one-field object fits.
        }
    }

    void main() {
        System.out.println("receiver made " + (a0 + a7));
    }
}
