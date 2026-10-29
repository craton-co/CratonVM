// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.*;
import java.lang.invoke.*;
import java.nio.*;
import java.util.concurrent.atomic.AtomicReference;

public class ByteBufferViewScopeProbe {
    static final VarHandle V = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
    static final int ROUNDS = Integer.getInteger("view.rounds", 4000);
    static void access(ByteBuffer b, int mode, int index) {
        switch (mode) {
            case 0: int a = (int) V.get(b, index); break;
            case 1: int c = (int) V.getOpaque(b, index); break;
            case 2: int d = (int) V.getAcquire(b, index); break;
            case 3: int e = (int) V.getVolatile(b, index); break;
            case 4: V.set(b, index, 1); break;
            case 5: V.setRelease(b, index, 1); break;
            case 6: boolean f = (boolean) V.compareAndSet(b, index, 1, 2); break;
            case 7: int g = (int) V.getAndAdd(b, index, 1); break;
            default: throw new AssertionError(mode);
        }
    }
    interface Action { void run() throws Throwable; }
    static String fault(Action r) {
        try { r.run(); return "MISSING"; }
        catch (Throwable e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    static void join(Thread t) throws Exception {
        t.join(10000);
        if (t.isAlive()) throw new AssertionError("worker did not stop");
    }
    static String offOwner(ByteBuffer b, int mode) throws Exception {
        AtomicReference<String> result = new AtomicReference<>();
        Thread t = new Thread(() -> result.set(fault(() -> access(b, mode, 0))));
        t.start(); join(t); return result.get();
    }
    static void priorities(ByteBuffer buffer, boolean closed) {
        for (int ro = 0; ro < 2; ro++) {
            ByteBuffer b = ro == 0 ? buffer : buffer.asReadOnlyBuffer();
            for (int index : new int[]{0, 1, 32}) for (int mode = 0; mode < 8; mode++) {
                final int m = mode;
                System.out.println("priority/" + closed + "/" + ro + "/" + index + "/" + m
                        + "=" + fault(() -> access(b, m, index)));
            }
        }
    }
    public static void main(String[] args) throws Exception {
        Arena arena = Arena.ofConfined();
        ByteBuffer b = arena.allocate(32, 8).asByteBuffer();
        for (int n = 0; n < ROUNDS; n++) {
            for (int mode = 0; mode < 8; mode++) access(b, mode, 0);
        }
        priorities(b, false);
        System.gc();
        for (int mode = 0; mode < 8; mode++) {
            System.out.println("off-owner/" + mode + "=" + offOwner(b, mode));
        }
        arena.close();
        priorities(b, true);
        for (int mode = 0; mode < 8; mode++) {
            final int m = mode;
            System.out.println("closed/" + m + "=" + fault(() -> access(b, m, 0)));
            System.out.println("closed-bounds/" + m + "=" + fault(() -> access(b, m, 32)));
            System.out.println("closed-off-owner/" + m + "=" + offOwner(b, m));
        }
        {
            Arena shared = Arena.ofShared();
            ByteBuffer s = shared.allocate(32, 8).asByteBuffer();
            AtomicReference<String> state = new AtomicReference<>();
            Thread t = new Thread(() -> {
                try {
                    for (int n = 0; n < ROUNDS; n++) for (int m = 0; m < 8; m++) access(s, m, 0);
                    state.set("ready");
                    for (;;) for (int m = 0; m < 8; m++) access(s, m, 0);
                } catch (IllegalStateException e) { state.set("closed"); }
                catch (Throwable e) { state.set(e.toString()); }
            });
            t.start();
            long deadline = System.nanoTime() + 30_000_000_000L;
            while (state.get() == null && System.nanoTime() < deadline) Thread.sleep(1);
            if (!"ready".equals(state.get())) throw new AssertionError("warmup=" + state.get());
            shared.close(); join(t);
            System.out.println("shared=" + state.get());
        }
        System.out.println("BYTE-BUFFER-VIEW-SCOPE-OK");
    }
}
