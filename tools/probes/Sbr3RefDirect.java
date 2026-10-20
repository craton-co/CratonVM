// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.ByteBuffer;

/**
 * SBR round 3, lane {@code ref}: {@code jdk.internal.ref.Cleaner} (a phantom that RUNS) discovered at
 * allocation. {@code DirectByteBuffer}'s whole reclamation path is one of these, so churning more
 * direct memory than the cap allows only succeeds if dropped buffers are reclaimed.
 *
 * <pre>
 *   cratonvm --jdk-only -Xmx256m -XX:MaxDirectMemorySize=128m -cp . Sbr3RefDirect
 *   java -Xmx256m -XX:MaxDirectMemorySize=128m -cp . Sbr3RefDirect
 * </pre>
 */
public final class Sbr3RefDirect {
    public static void main(String[] args) {
        long total = 0;
        int n = 0;
        try {
            for (int i = 0; i < 6000; i++) {
                ByteBuffer b = ByteBuffer.allocateDirect(64 * 1024);
                b.put(0, (byte) i);
                total += b.capacity();
                n++;
            }
            System.out.println("churned " + (total >> 20) + " MiB through a 128 MiB cap in " + n + " buffers");
        } catch (OutOfMemoryError e) {
            System.out.println("OutOfMemoryError after " + n + " buffers: " + e.getMessage());
        }
        System.out.println("PROBE-DONE");
    }
}
