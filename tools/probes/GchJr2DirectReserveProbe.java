// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.ByteBuffer;

/**
 * gch h2/jr (2026-10-08): {@code java.nio.Bits.reserveMemory}'s retry under a
 * direct-memory limit. With {@code -XX:MaxDirectMemorySize=16m} the loop
 * allocates 256 direct buffers of 1 MiB and drops each at once, so the 17th
 * reservation fails its fast path and the JDK retries: it asks
 * {@code JavaLangRefAccess.waitForReferenceProcessing()} (the JDK 25 body of
 * {@code Reference.waitForReferenceProcessing}: {@code processPendingLock} and
 * {@code hasReferencePendingList()}), calls {@code System.gc()}, and waits for
 * the buffers' {@code Cleaner}s to free the memory before it gives up with
 * {@code OutOfMemoryError: Cannot reserve ... direct buffer memory}.
 *
 * <p>The row this decides: with the {@code java/lang/ref} constructors and
 * {@code waitForReferenceProcessing} running as the JDK's bytecode (both modes
 * over a real JDK), a VM whose collector frees direct buffers through its own
 * delivery path must still let the retry succeed -- also under
 * {@code CRATONVM_FINALIZER_THREAD=1}, where the Cleaner runs on the delivery
 * thread and the JDK body never asks it.
 *
 * <p>Stdout (HotSpot 25 {@code -XX:+UseSerialGC -Xmx64m
 * -XX:MaxDirectMemorySize=16m} prints it):
 * <pre>
 *   direct-reserve: ok rounds=256 sum=32640
 * </pre>
 * On failure: {@code direct-reserve: OOME after N} (N buffers succeeded). The
 * elapsed time is on stderr.
 */
public final class GchJr2DirectReserveProbe {
    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 256;
        int size = 1 << 20;
        long sum = 0;
        int done = 0;
        long t0 = System.nanoTime();
        try {
            for (int i = 0; i < rounds; i++) {
                ByteBuffer b = ByteBuffer.allocateDirect(size);
                b.put(size - 1, (byte) i);
                sum += b.get(size - 1) & 0xff;
                done++;
            }
            System.out.println("direct-reserve: ok rounds=" + rounds + " sum=" + sum);
        } catch (OutOfMemoryError e) {
            System.out.println("direct-reserve: OOME after " + done);
            System.err.println("[direct-reserve] " + e);
        }
        System.err.println("[direct-reserve] ms=" + (System.nanoTime() - t0) / 1_000_000L);
    }
}
