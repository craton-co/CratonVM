// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.*;
import java.nio.*;
import java.util.concurrent.atomic.AtomicReference;

public final class ScopedByteAccessProbe {
    static void roundTrip(ByteBuffer b) {
        for (int i = 0; i < 20000; i++) {
            byte v = (byte) (i * 31);
            b.put(0, v);
            if (b.get(0) != v) throw new AssertionError("byte signedness or store width");
        }
        b.put(1, (byte) 72);
        b.put(0, (byte) -19);
        if (b.get(1) != 72) throw new AssertionError("neighbour overwritten");
        b.position(0); b.put((byte) -117); b.position(0);
        if (b.get() != -117) throw new AssertionError("relative byte access");
    }
    static String thrown(Runnable r) {
        try { r.run(); return "MISSING"; }
        catch (RuntimeException e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    public static void main(String[] args) throws Exception {
        roundTrip(ByteBuffer.allocate(32));
        roundTrip(ByteBuffer.allocateDirect(32));
        byte[] backing = new byte[32];
        roundTrip(MemorySegment.ofArray(backing).asByteBuffer());
        if (backing[0] != -117 || backing[1] != 72) throw new AssertionError("heap segment backing");
        Arena arena = Arena.ofConfined();
        ByteBuffer b = arena.allocate(32).asByteBuffer();
        roundTrip(b);
        ByteBuffer duplicate = b.duplicate();
        ByteBuffer slice = b.slice(0, 16);
        roundTrip(duplicate); roundTrip(slice);
        // Retire cached carrier/session layout proofs, then exercise them
        // again while alternating independent scopes and a heap-backed view.
        for (int i = 0; i < 3; i++) {
            System.gc();
            try (Arena temporary = Arena.ofConfined()) {
                roundTrip(temporary.allocate(32).asByteBuffer());
                roundTrip(b);
                roundTrip(MemorySegment.ofArray(backing).asByteBuffer());
            }
        }
        AtomicReference<String> offOwner = new AtomicReference<>();
        Thread other = new Thread(() -> offOwner.set(thrown(() -> b.get(0))));
        other.start(); other.join();
        System.out.println("off-owner=" + offOwner.get());
        System.out.println("readonly=" + thrown(() -> b.asReadOnlyBuffer().put(0, (byte) 1)));
        System.out.println("bounds=" + thrown(() -> b.put(32, (byte) 1)));
        arena.close();
        System.out.println("closed-read=" + thrown(() -> b.get(0)));
        System.out.println("closed-write=" + thrown(() -> duplicate.put(0, (byte) 1)));
        System.out.println("closed-slice=" + thrown(() -> slice.get(0)));
        Arena shared = Arena.ofShared();
        ByteBuffer s = shared.allocate(32).asByteBuffer();
        AtomicReference<String> sharedResult = new AtomicReference<>();
        Thread worker = new Thread(() -> {
            try {
                roundTrip(s);
                sharedResult.set("open");
                for (;;) s.get(0);
            } catch (IllegalStateException e) {
                sharedResult.set("closed");
            }
        });
        worker.start();
        long deadline = System.nanoTime() + 30_000_000_000L;
        while (sharedResult.get() == null && System.nanoTime() < deadline) Thread.sleep(1);
        if (!"open".equals(sharedResult.get())) throw new AssertionError("shared warmup");
        shared.close(); worker.join(10000);
        if (worker.isAlive() || !"closed".equals(sharedResult.get())) throw new AssertionError("shared close handshake");
        System.out.println("SCOPED-BYTE-ACCESS-OK");
    }
}
