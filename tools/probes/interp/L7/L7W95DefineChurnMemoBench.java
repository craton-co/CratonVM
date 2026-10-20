// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 95, lane L7
// (i94-L7-proposal-memos-stamped-with-the-definition-or-origin-epoch-retire-on-every-define,
// `CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH`, default off): the per-thread
// memos keyed by class id in `vm_exec.rs` -- the natives' by-name field-slot
// rings (`FIELD_NAME_RING`, `STATIC_NAME_RING`), the field-descriptor tiers'
// transient entries and the soft-reference touch memo (`SOFT_TOUCH_MEMO`,
// behind `SoftReference.get()`) -- are stamped with `class_origin_epoch`,
// which moves on every class definition in the process, so every define
// retires them on every thread. The switch stamps them with the store's
// hierarchy-edge epoch plus the in-place provenance epoch, which an unrelated
// definition does not move.
//
// Rows (ROUNDS = 400 rounds each; one op = one round):
//   define-only     define one fresh class through a custom loader (the
//                   define path's own natives read the loader's fields by
//                   name: the rings, retired by the previous define)
//   define-softget  the same, then CALLS = 64 `SoftReference.get()` over 8
//                   live soft references (the touch memo, retired by the
//                   define)
//   softget-only    the 64 gets with no define (the floor)
//   define-cas      define, then 64 `AtomicIntegerFieldUpdater` CASes on a
//                   real class's field (control: a durable descriptor entry,
//                   which no epoch retires; must not move)
//   cas-only        the 64 CASes with no define (the control's floor)
// The memo share of a define is (define-softget - define-only) - softget-only,
// plus whatever define-only itself loses.
//
// Run (--nojit: pure interpretation; each arm with the switch off and on):
//   javac -d out L7W95DefineChurnMemoBench.java
//   java -cp out L7W95DefineChurnMemoBench                                      (HotSpot reference)
//   cratonvm --jdk-only --nojit -cp out L7W95DefineChurnMemoBench
//   CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH=1 cratonvm --jdk-only --nojit -cp out L7W95DefineChurnMemoBench
//   (and the same pair with --compatible, where more receivers are stubs and
//   the descriptor tiers' transient entries matter)
// Interleave the arms, several rounds, medians. Companion rows:
// tools/probes/interp/L3/L3W91FieldNameRingBench.java (`*-interleaved` minus
// `*-batched`), tools/probes/interp/L4/L4W87DefineClassSplitBench.java and
// L4W90FinderScopeBench.java `find-fresh`.
//
// Output: "checksum <row> <n>" on stdout (deterministic); the timing line
// "row <row> min <ns> med <ns> ns/op" on stderr.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// checksum define-only 400
// checksum define-softget 25600
// checksum softget-only 25600
// checksum define-cas 25600
// checksum cas-only 25600
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.ref.SoftReference;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;

public class L7W95DefineChurnMemoBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final int ROUNDS = 400;
    static final int WARM = 40;
    static final int CALLS = 64;
    static int serial;

    static final class Cell {
        volatile int v;
    }

    static final AtomicIntegerFieldUpdater<Cell> V =
            AtomicIntegerFieldUpdater.newUpdater(Cell.class, "v");
    static final Cell CELL = new Cell();

    // Strongly held referents, so `get()` never answers null.
    static final Object[] REFERENTS = new Object[8];
    static final SoftReference<?>[] SOFT = new SoftReference<?>[8];

    static {
        for (int i = 0; i < SOFT.length; i++) {
            REFERENTS[i] = new int[] {i};
            SOFT[i] = new SoftReference<>(REFERENTS[i]);
        }
    }

    interface Row {
        int op(Definer loader);
    }

    static final class Definer extends ClassLoader {
        Definer() {
            super(L7W95DefineChurnMemoBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    static int defineOne(Definer loader) {
        String name = "L7W95Churn" + (serial++);
        Class<?> k = loader.define(name, build(name));
        return k.getName().equals(name) ? 1 : 0;
    }

    static int softGets() {
        SoftReference<?>[] s = SOFT;
        int n = 0;
        for (int i = 0; i < CALLS; i++) {
            if (s[i & 7].get() != null) {
                n++;
            }
        }
        return n;
    }

    static int cases() {
        Cell c = CELL;
        int n = 0;
        for (int i = 0; i < CALLS; i++) {
            int old = V.get(c);
            if (V.compareAndSet(c, old, old + 1)) {
                n++;
            }
        }
        return n;
    }

    static void time(String row, Definer loader, Row r) {
        for (int i = 0; i < WARM; i++) {
            r.op(loader);
        }
        long[] ns = new long[ROUNDS];
        long checksum = 0;
        for (int i = 0; i < ROUNDS; i++) {
            long t0 = System.nanoTime();
            checksum += r.op(loader);
            ns[i] = System.nanoTime() - t0;
        }
        Arrays.sort(ns);
        System.out.println("checksum " + row + " " + checksum);
        System.err.println("row " + row + " min " + ns[0] + " med " + ns[ROUNDS / 2] + " ns/op");
    }

    public static void main(String[] args) {
        Definer loader = new Definer();
        time("define-only", loader, L7W95DefineChurnMemoBench::defineOne);
        time("define-softget", loader, l -> {
            defineOne(l);
            return softGets();
        });
        time("softget-only", loader, l -> softGets());
        time("define-cas", loader, l -> {
            defineOne(l);
            return cases();
        });
        time("cas-only", loader, l -> cases());
    }
}
