// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane fuzz9: which objects are registered for
// finalization when finalizability comes from an ancestor, from a
// later-loaded definition in ANOTHER loader, or is cancelled by an EMPTY
// override (HotSpot's _has_empty_finalizer: a finalize() whose body is a lone
// `return` makes the class non-finalizable even under a finalizable super), or
// declared ABSTRACT (an abstract finalize() is not "empty", so the class is
// finalizable and the Finalizer thread swallows the AbstractMethodError).
// Base's constructor is compiled hot before any finalizable subclass exists.
//
// Shapes (slot: class, how):
//   0  Base                  plain
//   1  Fin extends Base       counting finalize
//   2  FinEmpty extends Fin   empty finalize
//   3  FinEmptyChild extends FinEmpty
//   4  FinEmptyCount extends FinEmpty, counting finalize
//   5  FinSuperCall extends Fin, finalize() { super.finalize(); }
//   6  AbsLeafEmpty extends AbsFin (abstract finalize), empty finalize
//   7  AbsLeafCount extends AbsFin, counting finalize
//   classfile-built, same names R21Fz9FlMid / R21Fz9FlLeaf in five child loaders:
//   8  L1 Leaf   Mid: no finalize            Leaf: none
//   9  L2 Leaf   Mid: counting finalize      Leaf: none
//   10 L3 Leaf   Mid: empty finalize         Leaf: counting
//   11 L4 Leaf   Mid: ABSTRACT class, abstract finalize; Leaf: none (concrete)
//   12 L5 Leaf   Mid: counting finalize      Leaf: empty
//   13 L1 Leaf again, after L2..L5 exist
//   14 L2 Mid    (the finalizable Mid itself)
//   15 L1 Mid
// Phase 1: N = 20000 objects per shape through one hot factory, then the
// settle loop of tools/probes/jit/r21/R21FinalizeAncestorCtor.java, then the
// finalize count per slot. Phase 2: 2000 more objects per shape whose finalize
// counts nothing (0 2 3 6 8 11 12 13 15), each with a
// PhantomReference, ONE System.gc(), then wait (no further GC): a plain object's
// phantom is enqueued by that GC, a finalizable one's is not (the FinalReference
// keeps it until finalize ran and a second GC). This makes registration
// visible where the finalizer itself counts nothing (empty / abstract).
//
// Risk targeted: wave 22's has_finalizer propagation to loaded subclasses
// (CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES) keyed by class, not name;
// the empty-finalizer rule; the init-return scan memo opt-in.
//
// Run:
//   javac -d out R21Fz9FinLater1.java
//   java -Xmx256m -cp out R21Fz9FinLater1                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz9FinLater1              # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz9FinLater1
//   cratonvm --nojit -Xmx256m -cp out R21Fz9FinLater1
//
// HotSpot 25 expectation (by reading ClassFileParser::parse_method and
// set_precomputed_flags: has_finalizer = !has_empty_finalizer &&
// (declares a non-empty finalize || super->has_finalizer()); not yet run):
//   fin  0 Base           finalized=0
//   fin  1 Fin            finalized=20000
//   fin  2 FinEmpty       finalized=0
//   fin  3 FinEmptyChild  finalized=0
//   fin  4 FinEmptyCount  finalized=20000
//   fin  5 FinSuperCall   finalized=20000
//   fin  6 AbsLeafEmpty   finalized=0
//   fin  7 AbsLeafCount   finalized=20000
//   fin  8 L1Leaf         finalized=0
//   fin  9 L2Leaf         finalized=20000
//   fin 10 L3Leaf         finalized=20000
//   fin 11 L4Leaf         finalized=0      (AbstractMethodError swallowed)
//   fin 12 L5Leaf         finalized=0
//   fin 13 L1LeafAgain    finalized=0
//   fin 14 L2Mid          finalized=20000
//   fin 15 L1Mid          finalized=0
//   reg  0 Base not registered
//   reg  2 FinEmpty not registered
//   reg  3 FinEmptyChild not registered
//   reg  6 AbsLeafEmpty not registered
//   reg  8 L1Leaf not registered
//   reg 11 L4Leaf registered
//   reg 12 L5Leaf not registered
//   reg 13 L1LeafAgain not registered
//   reg 15 L1Mid not registered
// The phantom check runs only for the shapes whose finalize counts NOTHING
// (for the counting shapes the finalized= count already shows registration,
// and a registered object that finalized early can be enqueued by the same
// GC cycle -- measured on HotSpot 25.0.3: rows 5 / 7 / 10 flipped between
// runs). Its only buckets: "not registered" = every phantom enqueued after the
// single GC; "registered" = anything less. Never a partial bucket.
// Before filing a short count, rerun: the settle loop may time out on a loaded
// machine.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicIntegerArray;

@SuppressWarnings("removal")
public class R21Fz9FinLater1 {
    static final int N = 20_000;
    static final int M = 2_000;
    static final int SHAPES = 16;
    // Shapes whose finalize counts nothing: only these get the phantom check.
    static final boolean[] PHANTOM_CHECKED = {true, false, true, true, false, false, true, false, true, false,
        false, true, true, true, false, true};
    static final AtomicIntegerArray FINS = new AtomicIntegerArray(SHAPES);
    static final String[] NAMES = {"Base", "Fin", "FinEmpty", "FinEmptyChild", "FinEmptyCount", "FinSuperCall",
        "AbsLeafEmpty", "AbsLeafCount", "L1Leaf", "L2Leaf", "L3Leaf", "L4Leaf", "L5Leaf", "L1LeafAgain", "L2Mid",
        "L1Mid"};

    public static void bump(int slot) {
        FINS.incrementAndGet(slot);
    }

    public static class Base {
        public int v;
        public int slot;

        public Base(int v, int slot) {
            this.v = v;
            this.slot = slot;
        }
    }

    public static class Fin extends Base {
        public Fin(int v, int slot) {
            super(v, slot);
        }

        @Override
        protected void finalize() {
            bump(slot);
        }
    }

    public static class FinEmpty extends Fin {
        public FinEmpty(int v, int slot) {
            super(v, slot);
        }

        @Override
        protected void finalize() {
        }
    }

    public static class FinEmptyChild extends FinEmpty {
        public FinEmptyChild(int v, int slot) {
            super(v, slot);
        }
    }

    public static class FinEmptyCount extends FinEmpty {
        public FinEmptyCount(int v, int slot) {
            super(v, slot);
        }

        @Override
        protected void finalize() {
            bump(slot);
        }
    }

    public static class FinSuperCall extends Fin {
        public FinSuperCall(int v, int slot) {
            super(v, slot);
        }

        @Override
        protected void finalize() {
            super.finalize();
        }
    }

    public abstract static class AbsFin extends Base {
        public AbsFin(int v, int slot) {
            super(v, slot);
        }

        @Override
        protected abstract void finalize() throws Throwable;
    }

    public static class AbsLeafEmpty extends AbsFin {
        public AbsLeafEmpty(int v, int slot) {
            super(v, slot);
        }

        @Override
        protected void finalize() {
        }
    }

    public static class AbsLeafCount extends AbsFin {
        public AbsLeafCount(int v, int slot) {
            super(v, slot);
        }

        @Override
        protected void finalize() {
            bump(slot);
        }
    }

    // ---- classfile-built Mid / Leaf ----

    static final ClassDesc HOST = ClassDesc.of("R21Fz9FinLater1");
    static final ClassDesc BASE = ClassDesc.of("R21Fz9FinLater1$Base");
    static final ClassDesc MID = ClassDesc.of("R21Fz9FlMid");
    static final ClassDesc LEAF = ClassDesc.of("R21Fz9FlLeaf");
    static final MethodTypeDesc MTD_II = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int,
            ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_I = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int);

    // fin: 0 none, 1 counting, 2 empty, 3 abstract (class becomes abstract)
    static byte[] build(ClassDesc self, ClassDesc sup, int fin) {
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER | (fin == 3 ? ClassFile.ACC_ABSTRACT : 0));
            cb.withSuperclass(sup);
            cb.withMethodBody(ConstantDescs.INIT_NAME, MTD_II, ClassFile.ACC_PUBLIC, c -> c
                    .aload(0).iload(1).iload(2)
                    .invokespecial(sup, ConstantDescs.INIT_NAME, MTD_II)
                    .return_());
            if (fin == 1) {
                cb.withMethodBody("finalize", ConstantDescs.MTD_void, ClassFile.ACC_PROTECTED, c -> c
                        .aload(0).getfield(BASE, "slot", ConstantDescs.CD_int)
                        .invokestatic(HOST, "bump", MTD_I)
                        .return_());
            } else if (fin == 2) {
                cb.withMethodBody("finalize", ConstantDescs.MTD_void, ClassFile.ACC_PROTECTED, c -> c.return_());
            } else if (fin == 3) {
                cb.withMethod("finalize", ConstantDescs.MTD_void, ClassFile.ACC_PROTECTED | ClassFile.ACC_ABSTRACT,
                        mb -> {
                        });
            }
        });
    }

    static final class Child extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Child(int midFin, int leafFin) {
            super(R21Fz9FinLater1.class.getClassLoader());
            bytes.put("R21Fz9FlMid", build(MID, BASE, midFin));
            bytes.put("R21Fz9FlLeaf", build(LEAF, MID, leafFin));
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

    static MethodHandle ctor(ClassLoader ld, String name) throws ReflectiveOperationException {
        Class<?> c = Class.forName(name, false, ld);
        return MethodHandles.publicLookup()
                .findConstructor(c, MethodType.methodType(void.class, int.class, int.class))
                .asType(MethodType.methodType(Base.class, int.class, int.class));
    }

    static final MethodHandle[] MH = new MethodHandle[SHAPES];

    static Base make(int shape, int i) throws Throwable {
        switch (shape) {
            case 0: return new Base(i, 0);
            case 1: return new Fin(i, 1);
            case 2: return new FinEmpty(i, 2);
            case 3: return new FinEmptyChild(i, 3);
            case 4: return new FinEmptyCount(i, 4);
            case 5: return new FinSuperCall(i, 5);
            case 6: return new AbsLeafEmpty(i, 6);
            case 7: return new AbsLeafCount(i, 7);
            default: return (Base) MH[shape].invokeExact(i, shape);
        }
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

    static int total() {
        int t = 0;
        for (int s = 0; s < SHAPES; s++) {
            t += FINS.get(s);
        }
        return t;
    }

    static int settle() {
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
        return total();
    }

    static long warmBase() {
        long s = 0;
        for (int r = 0; r < 100_000; r++) {
            s += new Base(r & 1023, 0).v;
        }
        return s;
    }

    static String pad(int s) {
        return (s < 10 ? " " : "") + s;
    }

    public static void main(String[] args) throws Throwable {
        System.out.println("warm " + warmBase());

        // Phase 1: the javac shapes first (each class loads at its first `new`).
        long sum = 0;
        for (int s = 0; s < 8; s++) {
            for (int i = 0; i < N; i++) {
                sum += make(s, i).v;
            }
        }
        Child l1 = new Child(0, 0);
        MH[8] = ctor(l1, "R21Fz9FlLeaf");
        for (int i = 0; i < N; i++) {
            sum += make(8, i).v;
        }
        Child l2 = new Child(1, 0);
        Child l3 = new Child(2, 1);
        Child l4 = new Child(3, 0);
        Child l5 = new Child(1, 2);
        MH[9] = ctor(l2, "R21Fz9FlLeaf");
        MH[10] = ctor(l3, "R21Fz9FlLeaf");
        MH[11] = ctor(l4, "R21Fz9FlLeaf");
        MH[12] = ctor(l5, "R21Fz9FlLeaf");
        MH[13] = MH[8];
        MH[14] = ctor(l2, "R21Fz9FlMid");
        MH[15] = ctor(l1, "R21Fz9FlMid");
        for (int s = 9; s < SHAPES; s++) {
            for (int i = 0; i < N; i++) {
                sum += make(s, i).v;
            }
        }
        System.out.println("made " + (sum == (long) SHAPES * N * (N - 1) / 2));
        settle();
        for (int s = 0; s < SHAPES; s++) {
            System.out.println("fin " + pad(s) + " " + NAMES[s] + " finalized=" + FINS.get(s));
        }

        // Phase 2: registration through phantom reachability after ONE gc.
        List<ReferenceQueue<Object>> queues = new ArrayList<>();
        List<Reference<?>> keep = new ArrayList<>();
        for (int s = 0; s < SHAPES; s++) {
            ReferenceQueue<Object> q = new ReferenceQueue<>();
            queues.add(q);
            for (int i = 0; PHANTOM_CHECKED[s] && i < M; i++) {
                keep.add(new PhantomReference<Object>(make(s, i), q));
            }
        }
        System.gc();
        int[] seen = new int[SHAPES];
        int lastTotal = -1;
        int still = 0;
        for (int round = 0; round < 2000; round++) {
            int tot = 0;
            for (int s = 0; s < SHAPES; s++) {
                while (queues.get(s).poll() != null) {
                    seen[s]++;
                }
                tot += seen[s];
            }
            if (tot == lastTotal) {
                still++;
            } else {
                still = 0;
                lastTotal = tot;
            }
            if (round >= 20 && still >= 40) {
                break;
            }
            try {
                Thread.sleep(5);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        for (int s = 0; s < SHAPES; s++) {
            if (!PHANTOM_CHECKED[s]) {
                continue;
            }
            String reg = seen[s] == M ? "not registered" : "registered";
            System.out.println("reg " + pad(s) + " " + NAMES[s] + " " + reg);
        }
        Reference.reachabilityFence(keep);
    }
}
