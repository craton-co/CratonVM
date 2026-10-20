// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * zs s4/nd4: the pre-{@code main} OutOfMemoryError of an INSTANCE main whose
 * {@code <clinit>} fills the heap to the last 16-byte object.
 *
 * <p>{@code Zs1NdLauncherOome} / {@code Zs2Nd2LauncherArgs} stop their fill at
 * the first refused {@code Object[2]} (24 bytes on HotSpot, 32 on CratonVM),
 * so on CratonVM's free-list arena a 16-31 byte hole often survives and serves
 * the 16-byte receiver: {@code main} is then ENTERED and the report is the
 * uncaught-dispatch path's (measured locally, zs s4/nd4), not the launcher's
 * {@code ExceptionDescribe}. HotSpot retires a thread's TLAB tail before a
 * refill, so the same hole never serves it. This probe removes that allocator
 * difference: after the chain fill it keeps allocating plain {@code Object}s
 * (16 bytes on both VMs) into a holder made BEFORE the fill, until one is
 * refused. Then no 16-byte request can be served on either VM and the
 * launcher's receiver fails on both.
 *
 * <p>HotSpot 25 ({@code -XX:+UseZGC -Xmx16m} and {@code -Xmx32m}): stdout
 * {@code start}, stderr exactly the 27 bytes {@code Exception in thread "main" }
 * (no newline), rc 1. {@code main-ran} on stdout means the holder ran out
 * before the heap did: not a verdict. Usage:
 * {@code java -Xmx16m Zs4Nd4LauncherExact}.
 */
public class Zs4Nd4LauncherExact {
    static Object[] chain;
    static Object[] smalls;

    static {
        System.out.println("start");
        smalls = new Object[1 << 16];
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
            Object[] holder = smalls;
            int n = 0;
            try {
                while (n < holder.length) {
                    holder[n] = new Object();
                    n++;
                }
            } catch (OutOfMemoryError e) {
                // The heap is full to the last 16 bytes.
            }
        } finally {
            chain = head;
        }
    }

    void main() {
        System.out.println("main-ran " + (chain != null));
    }
}
