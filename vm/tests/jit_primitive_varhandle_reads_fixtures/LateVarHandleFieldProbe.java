// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.invoke.*;

/** Resolves a lookup-created handle after its holder's clinit has finished. */
public final class LateVarHandleFieldProbe {
    static class IntBase {}
    static final class IntBox extends IntBase { int value; }
    static final class LongBox { long value; }
    static final VarHandle INT;
    static final VarHandle LONG;
    static final VarHandle EXACT;
    static {
        try {
            INT = MethodHandles.lookup().findVarHandle(IntBox.class, "value", int.class);
            LONG = MethodHandles.lookup().findVarHandle(LongBox.class, "value", long.class);
            EXACT = INT.withInvokeExactBehavior();
        } catch (ReflectiveOperationException e) { throw new ExceptionInInitializerError(e); }
        // Do not access either handle until this clinit and its plan feed end.
    }
    static int readInt(IntBox box) { return (int) INT.get(box); }
    static int readAcquireInt(IntBox box) { return (int) INT.getAcquire(box); }
    static long readLong(LongBox box) { return (long) LONG.get(box); }
    static long readVolatileLong(LongBox box) { return (long) LONG.getVolatile(box); }
    static int wrongExact(IntBase box) { return (int) EXACT.get(box); }
    static void awaitAcquire(IntBox box) {
        while (readAcquireInt(box) == 0) Thread.onSpinWait();
    }
    static String thrown(Runnable r) {
        try { r.run(); return "MISSING"; }
        catch (RuntimeException e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    public static void main(String[] args) throws Exception {
        IntBox i = new IntBox(); LongBox l = new LongBox();
        i.value = -17; l.value = -1234567890123L;
        int rounds = Integer.getInteger("late.rounds", 20000);
        for (int n = 0; n < rounds; n++) {
            if (readInt(i) != -17 || readAcquireInt(i) != -17 || readLong(l) != -1234567890123L
                    || readVolatileLong(l) != -1234567890123L) throw new AssertionError("warm read");
        }
        for (int n = 0; n < 4; n++) {
            System.gc(); i.value = Integer.MIN_VALUE; l.value = Long.MIN_VALUE;
            if (readInt(i) != Integer.MIN_VALUE || readAcquireInt(i) != Integer.MIN_VALUE
                    || readLong(l) != Long.MIN_VALUE || readVolatileLong(l) != Long.MIN_VALUE)
                throw new AssertionError("changed value, GC or wide sentinel");
        }
        if ((int) EXACT.get(i) != Integer.MIN_VALUE) throw new AssertionError("exact valid access");
        for (int n = 0; n < Math.min(rounds, 2000); n++) {
            String fault = thrown(() -> wrongExact(i));
            if (!fault.startsWith("java.lang.invoke.WrongMethodTypeException:"))
                throw new AssertionError("exact coordinate descriptor was not judged: " + fault);
        }
        System.out.println("wrong-exact=" + thrown(() -> wrongExact(i)));
        IntBox signal = new IntBox();
        Thread observer = new Thread(() -> awaitAcquire(signal));
        observer.setDaemon(true); observer.start();
        Thread.sleep(50);
        INT.setRelease(signal, 1);
        observer.join(10000);
        if (observer.isAlive()) throw new AssertionError("acquire read was hoisted or lost");
        System.out.println("acquire-observed=true");
        System.out.println("null-int=" + thrown(() -> readInt(null)));
        System.out.println("null-long=" + thrown(() -> readVolatileLong(null)));
        System.out.println("LATE-VARHANDLE-FIELD-OK");
    }
}
