// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 25, lane fuzz10: finalizable subclasses of a
// NON-finalizable base, constructed hot, whose constructor throws BEFORE the
// object reaches Object.<init> (an argument of super(..) / this(..) throws),
// INSIDE the base after Object.<init> returned, or AFTER super(..) returned.
// HotSpot registers the Finalizer when Object.<init> returns
// (RegisterFinalizersAtInit), so: thrown before -> never finalized; thrown in
// the base or after -> finalized, and finalize() sees the fields as they were
// when the constructor stopped (v = 0 when the base threw before storing it).
// Some subclasses are loaded only after Base.<init> is hot: a nested class
// first touched late, and classfile-built classes in a child loader.
//
// Slots (each 20000 constructions; finalize() adds 1 to finalized[slot] and
// v to vsum[slot]):
//   0 FinAfter        super(1, ok); then throw IllegalArgumentException("after")
//   1 FinInBase       super(1, mode 1): Base throws IllegalStateException("base")
//                     after Object.<init>, before storing v
//   2 FinBefore       super(boom(v), ..): boom throws IllegalStateException("before")
//   3 FinThisBefore   this(boom(v), 0) -> never reaches super
//   4 FinOk           normal
//   5 FinAlt          even i: boom before super; odd i: normal
//   6 FinCatchInside  super(1, ok); a throw caught inside the ctor; completes
//   7 FinAfterChild   extends FinAfter (own finalize): FinAfter's ctor throws after
//   8 LateBefore      classfile-built in loader "fz10f", loaded late: boom before super
//   9 LateAfter       classfile-built: super(v, 0, 0) then athrow "after"
//  10 LateInBase      classfile-built: super(v, 0, 1)
//  11 LateOk          classfile-built: normal
//  12 FinLateNested   like FinAfter, a nested class first touched in phase 2
// Phase 0: 200000 Base constructions (20000 of them throwing in the base), so
// Base.<init> is compiled before any finalizable subclass exists. Phase 1:
// slots 0-7 through one hot factory. Phase 2: slots 8-12 (loaded now) through
// constructor MethodHandles and a hot factory. Settle loop after each phase
// (System.gc + runFinalization until the count is stable).
//
// Risk targeted: finalizer registration on the compiled constructor paths of a
// non-finalizable base (b8bea1e7ad / 4d9c45daac: stub-upgrade finalizer flips,
// constructor gate rows dropped and recompiled when a class becomes
// finalizable), and the "unconstructed finalizable" withdrawal on a throw
// before Object.<init>.
//
// Run:
//   javac -d out R21Fz10FinCtorThrow1.java
//   java -Xmx256m -cp out R21Fz10FinCtorThrow1                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz10FinCtorThrow1              # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz10FinCtorThrow1
//   cratonvm --nojit -Xmx256m -cp out R21Fz10FinCtorThrow1
//
// HotSpot 25 expectation (JLS 12.6.1, HotSpot's RegisterFinalizersAtInit
// registration at Object.<init>'s return; not yet run):
//   warm sum=180000 thrown=20000
//   fin  0 FinAfter       thrown=20000 ex=java.lang.IllegalArgumentException: after finalized=20000 vsum=20000
//   fin  1 FinInBase      thrown=20000 ex=java.lang.IllegalStateException: base finalized=20000 vsum=0
//   fin  2 FinBefore      thrown=20000 ex=java.lang.IllegalStateException: before finalized=0 vsum=0
//   fin  3 FinThisBefore  thrown=20000 ex=java.lang.IllegalStateException: before finalized=0 vsum=0
//   fin  4 FinOk          thrown=0 ex=none finalized=20000 vsum=20000
//   fin  5 FinAlt         thrown=10000 ex=java.lang.IllegalStateException: before finalized=10000 vsum=10000
//   fin  6 FinCatchInside thrown=0 ex=none finalized=20000 vsum=20000
//   fin  7 FinAfterChild  thrown=20000 ex=java.lang.IllegalArgumentException: after finalized=20000 vsum=20000
//   fin  8 LateBefore     thrown=20000 ex=java.lang.IllegalStateException: before finalized=0 vsum=0
//   fin  9 LateAfter      thrown=20000 ex=java.lang.IllegalArgumentException: after finalized=20000 vsum=20000
//   fin 10 LateInBase     thrown=20000 ex=java.lang.IllegalStateException: base finalized=20000 vsum=0
//   fin 11 LateOk         thrown=0 ex=none finalized=20000 vsum=20000
//   fin 12 FinLateNested  thrown=20000 ex=java.lang.IllegalArgumentException: after finalized=20000 vsum=20000
// A short count may be the settle loop on a loaded machine: rerun before
// filing. A NON-zero count on 2, 3, 8 or the even half of 5 is never noise.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.atomic.AtomicIntegerArray;

@SuppressWarnings("removal")
public class R21Fz10FinCtorThrow1 {
    static final int N = 20_000;
    static final int SLOTS = 13;
    static final AtomicIntegerArray FINS = new AtomicIntegerArray(SLOTS);
    static final AtomicIntegerArray VSUM = new AtomicIntegerArray(SLOTS);
    static final int[] THROWN = new int[SLOTS];
    static final String[] EX = new String[SLOTS];
    static final String[] NAMES = {"FinAfter", "FinInBase", "FinBefore", "FinThisBefore", "FinOk", "FinAlt",
        "FinCatchInside", "FinAfterChild", "LateBefore", "LateAfter", "LateInBase", "LateOk", "FinLateNested"};

    public static void bump(int slot, int v) {
        FINS.incrementAndGet(slot);
        VSUM.addAndGet(slot, v);
    }

    /** Always throws (v >= 0 here); typed int so it can be a constructor argument. */
    public static int boom(int v) {
        if (v >= 0) {
            throw new IllegalStateException("before");
        }
        return v;
    }

    public static class Base {
        public int v;
        public int tag;

        public Base(int v, int tag, int mode) {
            if (mode == 1) {
                throw new IllegalStateException("base");
            }
            this.v = v;
            this.tag = tag;
        }
    }

    public static class FinAfter extends Base {
        public FinAfter(int v) {
            super(v, 0, 0);
            if (v >= 0) {
                throw new IllegalArgumentException("after");
            }
        }

        @Override
        protected void finalize() {
            bump(0, v);
        }
    }

    public static class FinInBase extends Base {
        public FinInBase(int v) {
            super(v, 1, 1);
        }

        @Override
        protected void finalize() {
            bump(1, v);
        }
    }

    public static class FinBefore extends Base {
        public FinBefore(int v) {
            super(boom(v), 2, 0);
        }

        @Override
        protected void finalize() {
            bump(2, v);
        }
    }

    public static class FinThisBefore extends Base {
        public FinThisBefore(int v) {
            this(boom(v), 0);
        }

        public FinThisBefore(int v, int unused) {
            super(v, 3, 0);
        }

        @Override
        protected void finalize() {
            bump(3, v);
        }
    }

    public static class FinOk extends Base {
        public FinOk(int v) {
            super(v, 4, 0);
        }

        @Override
        protected void finalize() {
            bump(4, v);
        }
    }

    public static class FinAlt extends Base {
        public FinAlt(int v, int i) {
            super((i & 1) == 0 ? boom(v) : v, 5, 0);
        }

        @Override
        protected void finalize() {
            bump(5, v);
        }
    }

    public static class FinCatchInside extends Base {
        public FinCatchInside(int v) {
            super(v, 6, 0);
            try {
                boom(v);
            } catch (IllegalStateException e) {
                tag = 66;
            }
        }

        @Override
        protected void finalize() {
            bump(6, v);
        }
    }

    public static class FinAfterChild extends FinAfter {
        public FinAfterChild(int v) {
            super(v);
        }

        @Override
        protected void finalize() {
            bump(7, v);
        }
    }

    // First touched in phase 2, after Base.<init> and the phase 1 factory are hot.
    public static class FinLateNested extends Base {
        public FinLateNested(int v) {
            super(v, 12, 0);
            if (v >= 0) {
                throw new IllegalArgumentException("after");
            }
        }

        @Override
        protected void finalize() {
            bump(12, v);
        }
    }

    // ---- classfile-built late subclasses ----

    static final ClassDesc HOST = ClassDesc.of("R21Fz10FinCtorThrow1");
    static final ClassDesc BASE = ClassDesc.of("R21Fz10FinCtorThrow1$Base");
    static final ClassDesc IAE = ClassDesc.of("java.lang.IllegalArgumentException");
    static final MethodTypeDesc MTD_III = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int,
            ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_I = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_II_V = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int,
            ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_I_I = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);

    // how: 0 boom before super, 1 throw after super, 2 base throws (mode 1), 3 normal
    static byte[] late(String name, int slot, int how) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(BASE);
            cb.withMethodBody(ConstantDescs.INIT_NAME, MTD_I, ClassFile.ACC_PUBLIC, c -> {
                c.aload(0).iload(1);
                if (how == 0) {
                    c.invokestatic(HOST, "boom", MTD_I_I);
                }
                c.bipush(slot).iconst_0();
                if (how == 2) {
                    c.pop().iconst_1();
                }
                c.invokespecial(BASE, ConstantDescs.INIT_NAME, MTD_III);
                if (how == 1) {
                    c.new_(IAE).dup().ldc("after")
                            .invokespecial(IAE, ConstantDescs.INIT_NAME,
                                    MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                            .athrow();
                } else {
                    c.return_();
                }
            });
            cb.withMethodBody("finalize", ConstantDescs.MTD_void, ClassFile.ACC_PROTECTED, c -> c
                    .bipush(slot).aload(0).getfield(BASE, "v", ConstantDescs.CD_int)
                    .invokestatic(HOST, "bump", MTD_II_V)
                    .return_());
        });
    }

    static final class Child extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Child() {
            super("fz10f", R21Fz10FinCtorThrow1.class.getClassLoader());
            bytes.put("fz10f.LateBefore", late("fz10f.LateBefore", 8, 0));
            bytes.put("fz10f.LateAfter", late("fz10f.LateAfter", 9, 1));
            bytes.put("fz10f.LateInBase", late("fz10f.LateInBase", 10, 2));
            bytes.put("fz10f.LateOk", late("fz10f.LateOk", 11, 3));
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    // ---- settle (as tools/probes/jit/r21/R21FinalizeAncestorCtor.java) ----

    static int total() {
        int s = 0;
        for (int i = 0; i < SLOTS; i++) {
            s += FINS.get(i);
        }
        return s;
    }

    static void collect() {
        System.gc();
        System.runFinalization();
        try {
            Thread.sleep(2);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    static void settle() {
        int last = -1;
        int still = 0;
        for (int i = 0; i < 400; i++) {
            collect();
            int now = total();
            if (now == last) {
                still++;
            } else {
                still = 0;
                last = now;
            }
            if (i >= 20 && still >= 10) {
                break;
            }
        }
    }

    // ---- factories ----

    static Object make(int slot, int i) {
        switch (slot) {
            case 0:
                return new FinAfter(1);
            case 1:
                return new FinInBase(1);
            case 2:
                return new FinBefore(1);
            case 3:
                return new FinThisBefore(1);
            case 4:
                return new FinOk(1);
            case 5:
                return new FinAlt(1, i);
            case 6:
                return new FinCatchInside(1);
            case 7:
                return new FinAfterChild(1);
            case 12:
                return new FinLateNested(1);
            default:
                throw new AssertionError(slot);
        }
    }

    static void note(int slot, Throwable t) {
        THROWN[slot]++;
        if (EX[slot] == null) {
            EX[slot] = t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void runSlot(int slot) {
        for (int i = 0; i < N; i++) {
            try {
                make(slot, i);
            } catch (RuntimeException e) {
                note(slot, e);
            }
        }
    }

    static void runMh(int slot, MethodHandle ctor) {
        for (int i = 0; i < N; i++) {
            try {
                Object o = ctor.invoke(1);
            } catch (Throwable t) {
                note(slot, t);
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        long sum = 0;
        int warmThrown = 0;
        for (int i = 0; i < 200_000; i++) {
            try {
                sum += new Base(1, 0, i % 10 == 9 ? 1 : 0).v;
            } catch (IllegalStateException e) {
                warmThrown++;
            }
        }
        System.out.println("warm sum=" + sum + " thrown=" + warmThrown);
        settle();

        // Phase 1: interleave the slots in blocks so every shape is hot in the same factory.
        for (int block = 0; block < 4; block++) {
            for (int slot = 0; slot <= 7; slot++) {
                for (int i = 0; i < N / 4; i++) {
                    try {
                        make(slot, block * (N / 4) + i);
                    } catch (RuntimeException e) {
                        note(slot, e);
                    }
                }
            }
        }
        settle();

        // Phase 2: late classes.
        Child ch = new Child();
        MethodHandles.Lookup l = MethodHandles.lookup();
        String[] late = {"fz10f.LateBefore", "fz10f.LateAfter", "fz10f.LateInBase", "fz10f.LateOk"};
        for (int k = 0; k < late.length; k++) {
            Class<?> c = Class.forName(late[k], true, ch);
            runMh(8 + k, l.findConstructor(c, MethodType.methodType(void.class, int.class)));
        }
        runSlot(12);
        settle();

        for (int slot = 0; slot < SLOTS; slot++) {
            System.out.println(String.format("fin %2d %-14s thrown=%d ex=%s finalized=%d vsum=%d", slot, NAMES[slot],
                    THROWN[slot], EX[slot] == null ? "none" : EX[slot], FINS.get(slot), VSUM.get(slot)));
        }
    }
}
