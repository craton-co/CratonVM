// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.*;
import java.nio.*;
import java.util.concurrent.atomic.AtomicReference;

public final class ScopedWideAccessProbe {
    static void roundTrip(ByteBuffer b) {
        for (ByteOrder order : new ByteOrder[]{ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN}) {
            b.order(order);
            for (int i = 0; i < 1000; i++) {
                long value = 0xFEDCBA9876543210L ^ i;
                b.putLong(1, value);
                if (b.getLong(1) != value) throw new AssertionError("long/order");
                b.putInt(1, (int) value);
                if (b.getInt(1) != (int) value) throw new AssertionError("int/order");
                b.putShort(1, (short) value);
                if (b.getShort(1) != (short) value) throw new AssertionError("short/sign/order");
                b.putChar(1, (char) value);
                if (b.getChar(1) != (char) value) throw new AssertionError("char/unsigned/order");
                b.putDouble(1, Double.longBitsToDouble(value));
                if (Double.doubleToRawLongBits(b.getDouble(1)) != value) throw new AssertionError("double/bits");
                b.putFloat(1, Float.intBitsToFloat((int) value));
                if (Float.floatToRawIntBits(b.getFloat(1)) != (int) value) throw new AssertionError("float/bits");
            }
            b.putLong(1, 0xFEDCBA9876543210L);
            for (int i = 0; i < 8; i++) {
                int shift = order == ByteOrder.BIG_ENDIAN ? 8 * (7 - i) : 8 * i;
                if (b.get(1 + i) != (byte) (0xFEDCBA9876543210L >>> shift)) throw new AssertionError("raw order");
            }
            b.putLong(1, Long.MIN_VALUE);
            if (b.getLong(1) != Long.MIN_VALUE) throw new AssertionError("long minimum/sentinel");
            b.putInt(1, Integer.MIN_VALUE);
            if (b.getInt(1) != Integer.MIN_VALUE) throw new AssertionError("int minimum");
            b.putShort(1, Short.MIN_VALUE);
            if (b.getShort(1) != Short.MIN_VALUE) throw new AssertionError("short minimum");
            b.putChar(1, '\uFFFF');
            if (b.getChar(1) != '\uFFFF') throw new AssertionError("char maximum");
            b.position(0); b.putLong(-17L); b.position(0);
            if (b.getLong() != -17L) throw new AssertionError("relative long");
            b.put(9, (byte) 71); b.putLong(1, -1L);
            if (b.get(9) != 71) throw new AssertionError("neighbour overwritten");
        }
    }
    static String thrown(Runnable r) {
        try { r.run(); return "MISSING"; }
        catch (RuntimeException e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    public static void main(String[] args) throws Exception {
        roundTrip(ByteBuffer.allocate(64));
        roundTrip(ByteBuffer.allocateDirect(64));
        byte[] backing = new byte[64];
        roundTrip(MemorySegment.ofArray(backing).asByteBuffer());
        try (Arena arena = Arena.ofConfined()) {
            ByteBuffer b = arena.allocate(64).asByteBuffer();
            roundTrip(b); roundTrip(b.duplicate()); roundTrip(b.slice(0, 32));
            for (int i = 0; i < 3; i++) {
                System.gc();
                try (Arena temporary = Arena.ofConfined()) {
                    roundTrip(temporary.allocate(64).asByteBuffer()); roundTrip(b);
                }
            }
            AtomicReference<String> result = new AtomicReference<>();
            Thread other = new Thread(() -> result.set(thrown(() -> b.getLong(1))));
            other.start(); other.join();
            System.out.println("off-owner=" + result.get());
            System.out.println("readonly=" + thrown(() -> b.asReadOnlyBuffer().putChar(1, '\uFFFF')));
            System.out.println("bounds=" + thrown(() -> b.putLong(57, 1)));
        }
        Arena closed = Arena.ofConfined();
        ByteBuffer b = closed.allocate(64).asByteBuffer(); roundTrip(b);
        ByteBuffer alias = b.duplicate(); closed.close();
        System.out.println("closed-read=" + thrown(() -> b.getShort(1)));
        System.out.println("closed-write=" + thrown(() -> alias.putInt(1, 1)));
        Arena shared = Arena.ofShared();
        ByteBuffer s = shared.allocate(64).asByteBuffer();
        AtomicReference<String> result = new AtomicReference<>();
        Thread worker = new Thread(() -> {
            try {
                roundTrip(s); result.set("open");
                for (;;) { s.putLong(1, -1L); s.getChar(1); }
            } catch (IllegalStateException e) { result.set("closed"); }
        });
        worker.start();
        long deadline = System.nanoTime() + 30_000_000_000L;
        while (result.get() == null && System.nanoTime() < deadline) Thread.sleep(1);
        if (!"open".equals(result.get())) throw new AssertionError("shared warmup");
        shared.close(); worker.join(10000);
        if (worker.isAlive() || !"closed".equals(result.get())) throw new AssertionError("shared close handshake");
        System.out.println("SCOPED-WIDE-ACCESS-OK");
    }
}
