// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 1 (lane FFMVH): every access mode a narrow (byte / short / char / boolean)
// or unaligned segment VarHandle does NOT declare must throw
// UnsupportedOperationException on CratonVM as on HotSpot 25, in both modes, and the
// modes it does declare must still work. Page
// docs/known-issues/jit/ffm-segment-varhandle-short-compare-and-set-overflows-the-stack-20261027.md
// (the short / char CAS family recursed to a SIGSEGV).
// Each row is run twice (cold, then 2000 more times) so a compiled door is covered too;
// the fact is the first answer, plus "stable" when every repeat answered the same.
//   java J27FfmVhNarrowModes
import java.lang.foreign.*;
import java.lang.invoke.VarHandle;

public class J27FfmVhNarrowModes {
    interface Row { Object run(MemorySegment s) throws Throwable; }

    static void fact(String name, Row r) {
        String first = null;
        boolean stable = true;
        for (int i = 0; i < 2001; i++) {
            String out;
            try (Arena a = Arena.ofConfined()) {
                MemorySegment s = a.allocate(16, 8);
                try { out = String.valueOf(r.run(s)); } catch (Throwable t) { out = "THREW " + t; }
            }
            if (first == null) first = out; else if (!first.equals(out)) stable = false;
        }
        System.out.println("fact " + name + " " + first.replace(' ', '_') + (stable ? "" : "_UNSTABLE"));
    }

    static final VarHandle B = ValueLayout.JAVA_BYTE.varHandle();
    static final VarHandle S = ValueLayout.JAVA_SHORT.varHandle();
    static final VarHandle C = ValueLayout.JAVA_CHAR.varHandle();
    static final VarHandle Z = ValueLayout.JAVA_BOOLEAN.varHandle();
    static final VarHandle IU = ValueLayout.JAVA_INT_UNALIGNED.varHandle();
    static final VarHandle F = ValueLayout.JAVA_FLOAT.varHandle();
    static final VarHandle I = ValueLayout.JAVA_INT.varHandle();

    public static void main(String[] args) {
        fact("short.cas", s -> S.compareAndSet(s, 0L, (short) 0, (short) 2));
        fact("short.cae", s -> (short) S.compareAndExchange(s, 0L, (short) 0, (short) 2));
        fact("short.wcas", s -> S.weakCompareAndSetPlain(s, 0L, (short) 0, (short) 2));
        fact("short.getAndSet", s -> (short) S.getAndSet(s, 0L, (short) 2));
        fact("short.getAndAdd", s -> (short) S.getAndAdd(s, 0L, (short) 2));
        fact("char.cas", s -> C.compareAndSet(s, 0L, (char) 0, 'b'));
        fact("char.getAndSetAcquire", s -> (char) C.getAndSetAcquire(s, 0L, 'b'));
        fact("char.getAndBitwiseOr", s -> (char) C.getAndBitwiseOr(s, 0L, 'b'));
        fact("byte.cae", s -> (byte) B.compareAndExchange(s, 0L, (byte) 0, (byte) 2));
        fact("boolean.cas", s -> Z.compareAndSet(s, 0L, false, true));
        fact("intUnaligned.cas", s -> IU.compareAndSet(s, 0L, 0, 2));
        fact("intUnaligned.getVolatile", s -> (int) IU.getVolatile(s, 0L));
        fact("float.getAndAdd", s -> (float) F.getAndAdd(s, 0L, 1.0f));
        // Declared modes still work.
        fact("short.volatile", s -> { S.setVolatile(s, 0L, (short) 7); return (short) S.getAcquire(s, 0L); });
        fact("char.opaque", s -> { C.setOpaque(s, 2L, 'q'); return (char) C.getOpaque(s, 2L); });
        fact("float.getAndSet", s -> { F.set(s, 4L, 1.5f); return (float) F.getAndSet(s, 4L, 2.5f) + "/" + (float) F.get(s, 4L); });
        fact("int.cas", s -> I.compareAndSet(s, 8L, 0, 5) + "/" + (int) I.get(s, 8L));
        // A refused CAS on a read-only segment: the JDK's own check answers.
        fact("int.cas.readOnly", s -> I.compareAndSet(s.asReadOnly(), 0L, 0, 5));
        staticVhRows();
        System.out.println("DONE");
    }

    // j27 wave 1 (lane FFMVH): static-field VarHandle read-modify-writes (the
    // natives' static arms, CRATONVM_VH_STATIC_SLOT_MEMO). Facts are deterministic;
    // the timing row prints only with -Dj27.time=true.
    static int SI;
    static long SL;
    static class Lazy { static int LI = 40; }
    static final VarHandle VH_SI, VH_SL, VH_LAZY;
    static {
        try {
            java.lang.invoke.MethodHandles.Lookup l = java.lang.invoke.MethodHandles.lookup();
            VH_SI = l.findStaticVarHandle(J27FfmVhNarrowModes.class, "SI", int.class);
            VH_SL = l.findStaticVarHandle(J27FfmVhNarrowModes.class, "SL", long.class);
            VH_LAZY = l.findStaticVarHandle(Lazy.class, "LI", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static void staticVhRows() {
        SI = 0;
        for (int i = 0; i < 80000; i++) VH_SI.getAndAdd(1);
        System.out.println("fact static-vh-int-xadd " + SI);
        int ok = 0;
        for (int i = 0; i < 1000; i++) if (VH_SI.compareAndSet(80000 + i, 80001 + i)) ok++;
        System.out.println("fact static-vh-int-cas " + ok + "/" + SI);
        System.out.println("fact static-vh-int-getAndSet " + (int) VH_SI.getAndSet(7) + "/" + SI);
        System.out.println("fact static-vh-int-or " + (int) VH_SI.getAndBitwiseOr(8) + "/" + SI);
        System.out.println("fact static-vh-int-cae " + (int) VH_SI.compareAndExchange(15, 3) + "/" + (int) VH_SI.get());
        SL = 1L << 40;
        for (int i = 0; i < 1000; i++) VH_SL.getAndAdd(3L);
        System.out.println("fact static-vh-long-xadd " + SL);
        VH_SL.set(5L);
        System.out.println("fact static-vh-long-set-get " + (long) VH_SL.getVolatile());
        // The first access initializes the holder (40 from its <clinit>), later ones hit.
        System.out.println("fact static-vh-lazy-holder " + (int) VH_LAZY.getAndAdd(2) + "/" + (int) VH_LAZY.get());
        if (Boolean.getBoolean("j27.time")) {
            for (int rep = 0; rep < 5; rep++) {
                int n = 2_000_000;
                long t0 = System.nanoTime();
                for (int i = 0; i < n; i++) VH_SI.getAndAdd(1);
                long t1 = System.nanoTime();
                System.out.println("time static-vh-int-xadd " + ((t1 - t0) / n) + " ns/op");
            }
        }
    }
}
