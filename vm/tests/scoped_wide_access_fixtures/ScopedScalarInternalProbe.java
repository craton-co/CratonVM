// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.*;
import java.util.concurrent.atomic.AtomicReference;
import jdk.internal.misc.ScopedMemoryAccess;
import jdk.internal.misc.Unsafe;
import jdk.internal.foreign.MemorySessionImpl;

/** Exercises every reviewed scalar Internal through its authoritative Java wrapper. */
public final class ScopedScalarInternalProbe {
    static final ScopedMemoryAccess S = ScopedMemoryAccess.getScopedMemoryAccess();
    static final Unsafe U = Unsafe.getUnsafe();
    static final int ROUNDS = Integer.getInteger("scoped.rounds", 12000);

    static void access(MemorySessionImpl session, Object base, long offset) {
        S.putByte(session, base, offset + 9, (byte) 73);
        S.putByte(session, base, offset, (byte) -128);
        if (S.getByte(session, base, offset) != -128) throw new AssertionError("byte sign");
        S.putShort(session, base, offset, Short.MIN_VALUE);
        if (S.getShort(session, base, offset) != Short.MIN_VALUE) throw new AssertionError("short sign");
        S.putChar(session, base, offset, '\uffff');
        if (S.getChar(session, base, offset) != '\uffff') throw new AssertionError("char unsigned");
        S.putInt(session, base, offset, Integer.MIN_VALUE);
        if (S.getInt(session, base, offset) != Integer.MIN_VALUE) throw new AssertionError("int sign");
        S.putLong(session, base, offset, Long.MIN_VALUE);
        if (S.getLong(session, base, offset) != Long.MIN_VALUE) throw new AssertionError("long sentinel");
        for (int order = 0; order < 2; order++) {
            boolean be = order != 0;
            long at = offset + 1;
            S.putLongUnaligned(session, base, at, 0xfedcba9876543210L, be);
            if (S.getLongUnaligned(session, base, at, be) != 0xfedcba9876543210L) throw new AssertionError("long order");
            for (int i = 0; i < 8; i++) {
                int shift = be ? 8 * (7-i) : 8 * i;
                if (S.getByte(session, base, at+i) != (byte)(0xfedcba9876543210L >>> shift)) throw new AssertionError("raw order");
            }
            S.putShortUnaligned(session, base, at, Short.MIN_VALUE, be);
            if (S.getShortUnaligned(session, base, at, be) != Short.MIN_VALUE) throw new AssertionError("short unaligned");
            S.putCharUnaligned(session, base, at, '\uffff', be);
            if (S.getCharUnaligned(session, base, at, be) != '\uffff') throw new AssertionError("char unaligned");
            S.putIntUnaligned(session, base, at, Integer.MIN_VALUE, be);
            if (S.getIntUnaligned(session, base, at, be) != Integer.MIN_VALUE) throw new AssertionError("int unaligned");
            S.putLongUnaligned(session, base, at, Long.MIN_VALUE, be);
            if (S.getLongUnaligned(session, base, at, be) != Long.MIN_VALUE) throw new AssertionError("long unaligned sentinel");
        }
        // Unaligned long ends at offset+8; the next byte must remain untouched.
        S.putLong(session, base, offset, -1L);
        if (S.getByte(session, base, offset + 9) != 73) throw new AssertionError("store width");
    }
    static void warm(MemorySessionImpl session, Object base, long offset) {
        for (int i = 0; i < ROUNDS; i++) access(session, base, offset);
    }
    static String thrown(Runnable r) {
        try { r.run(); return "MISSING"; }
        catch (RuntimeException e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    public static void main(String[] args) throws Exception {
        // A read may span several elements of any primitive carrier, including NaN bits.
        for (Object base : new Object[]{new boolean[64], new byte[64], new short[32], new char[32], new int[16],
                new long[8], new float[16], new double[8]}) {
            warm(null, base, U.arrayBaseOffset(base.getClass()));
            System.gc();
            access(null, base, U.arrayBaseOffset(base.getClass()));
        }
        // Unsafe raw writes preserve all bits; ordinary Java boolean stores
        // still use their own canonical bastore representation.
        boolean[] flags = new boolean[1];
        long flagOffset = U.arrayBaseOffset(boolean[].class);
        for (int b : new int[]{0, 1, 2, 127, -128, -1}) {
            U.putByte(flags, flagOffset, (byte) b);
            System.out.println("boolean-wire=" + b + ":" + U.getByte(flags, flagOffset)
                    + ":" + U.getBoolean(flags, flagOffset) + ":" + flags[0]);
        }
        flags[0] = true;
        if (U.getByte(flags, flagOffset) != 1) throw new AssertionError("boolean true store");
        flags[0] = false;
        if (U.getByte(flags, flagOffset) != 0) throw new AssertionError("boolean false store");
        // Cross a G1 region boundary in a humongous primitive carrier.
        boolean[] largeFlags = new boolean[2_100_000];
        access(null, largeFlags, U.arrayBaseOffset(boolean[].class) + 1_048_573);
        System.gc();
        access(null, largeFlags, U.arrayBaseOffset(boolean[].class) + 2_099_980);
        Arena confined = Arena.ofConfined();
        MemorySegment block = confined.allocate(64);
        MemorySessionImpl session = (MemorySessionImpl) block.scope();
        long address = block.address();
        warm(session, null, address);
        AtomicReference<String> wrong = new AtomicReference<>();
        Thread peer = new Thread(() -> wrong.set(thrown(() -> S.getLong(session, null, address))));
        peer.start(); peer.join();
        System.out.println("off-owner=" + wrong.get());
        confined.close();
        Thread closedPeer = new Thread(() -> wrong.set(thrown(() -> S.getLong(session, null, address))));
        closedPeer.start(); closedPeer.join();
        System.out.println("closed-off-owner=" + wrong.get());
        System.out.println("closed-read=" + thrown(() -> S.getByte(session, null, address)));
        System.out.println("closed-write=" + thrown(() -> S.putCharUnaligned(session, null, address+1, 'x', true)));
        try (Arena destination = Arena.ofConfined()) {
            MemorySegment dst = destination.allocate(64);
            MemorySessionImpl ds = (MemorySessionImpl) dst.scope();
            System.out.println("copy-closed-source=" + thrown(() -> S.copyMemory(session, ds, null, address, null, dst.address(), 8)));
            System.out.println("copy-closed-destination=" + thrown(() -> S.copyMemory(ds, session, null, dst.address(), null, address, 8)));
        }
        Arena shared = Arena.ofShared();
        MemorySegment sh = shared.allocate(64);
        MemorySessionImpl ss = (MemorySessionImpl) sh.scope();
        long sa = sh.address();
        AtomicReference<String> result = new AtomicReference<>();
        Thread worker = new Thread(() -> {
            try {
                warm(ss, null, sa); result.set("open");
                for (;;) access(ss, null, sa);
            } catch (IllegalStateException e) { result.set("closed"); }
        });
        worker.start();
        long deadline = System.nanoTime() + 30_000_000_000L;
        while (result.get() == null && System.nanoTime() < deadline) Thread.sleep(1);
        if (!"open".equals(result.get())) throw new AssertionError("shared warmup");
        shared.close(); worker.join(10000);
        if (worker.isAlive() || !"closed".equals(result.get())) throw new AssertionError("shared close");
        System.out.println("SCOPED-SCALAR-INTERNAL-OK");
    }
}
