// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 18, lane review-w18: the empty-finalize rule
// (`CRATONVM_FINALIZER_EMPTY_IS_NONE`, wave 17,
// `classloading::class_manager::class_has_finalizer_by_hotspot_rule`).
//
// Risk targeted (docs/known-issues/jit/r21-review-w18-findings-20261019.md):
// HotSpot 25's ClassFileParser treats a declared `finalize()V` whose code is
// a lone `return` as no finalizer, and that empty override CANCELS the
// finalizer the class would inherit (`set_precomputed_flags`: has_finalizer
// only when `!_has_empty_finalizer && (_has_finalizer ||
// super->has_finalizer())`). A finalizable object needs one more collection
// before a PhantomReference to it is enqueued (its finalizer must run first),
// so the number of `System.gc()` calls before the phantom arrives says
// whether the VM registered the object: `1` = not finalizable, `2+` =
// finalizable. The allocations run cold first, then from a hot (compiled)
// allocation site, so the interpreter's and the JIT's `new` are both covered.
//
// Classes:
//   Plain        no finalize                               -> 1
//   Fin          finalize() { RAN++ }                       -> 2+
//   EmptySub     extends Fin, finalize() {}                 -> 1  (cancels Fin's)
//   EmptyLeaf    extends EmptySub, no declaration           -> 1  (inherits none)
//   SuperCaller  extends EmptySub, finalize() { super.finalize(); } -> 2+
//
// Passes: `cold` (each kind once, first allocation of each class), `warm`
// (60k allocations of the non-finalizable kinds through one site), `hot` (each
// kind again, now allocated by the compiled site), `fin` (Fin's finalizer ran).
//
// Run:
//   javac -d out R21Fz6EmptyFinalize1.java
//   java -Xmx256m -cp out R21Fz6EmptyFinalize1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz6EmptyFinalize1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz6EmptyFinalize1
//   cratonvm --nojit -Xmx256m -cp out R21Fz6EmptyFinalize1
//   CRATONVM_FINALIZER_EMPTY_IS_NONE=0 cratonvm ...    # control arm: EmptySub/EmptyLeaf say 2+
// HotSpot expectation: every line identical on every arm except the control
// arm. javac warns that finalize() is deprecated for removal; that warning is
// expected.

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.util.concurrent.atomic.AtomicInteger;

@SuppressWarnings({"removal", "deprecation"})
public class R21Fz6EmptyFinalize1 {
    static final AtomicInteger RAN = new AtomicInteger();
    static final String[] KINDS = {"Plain", "Fin", "EmptySub", "EmptyLeaf", "SuperCaller"};
    static long sink;

    static class Plain {
        final int v;

        Plain(int v) {
            this.v = v;
        }
    }

    static class Fin {
        final int v;

        Fin(int v) {
            this.v = v;
        }

        @Override
        protected void finalize() {
            RAN.incrementAndGet();
        }
    }

    static class EmptySub extends Fin {
        EmptySub(int v) {
            super(v);
        }

        // javac: a lone `return` (Method::is_empty_method).
        @Override
        protected void finalize() {
        }
    }

    static class EmptyLeaf extends EmptySub {
        EmptyLeaf(int v) {
            super(v);
        }
    }

    static class SuperCaller extends EmptySub {
        SuperCaller(int v) {
            super(v);
        }

        // `aload_0; invokespecial EmptySub.finalize; return`: not empty.
        @Override
        protected void finalize() {
            super.finalize();
        }
    }

    // The one allocation site of every kind.
    static Object alloc(int kind, int v) {
        switch (kind) {
            case 0:
                return new Plain(v);
            case 1:
                return new Fin(v);
            case 2:
                return new EmptySub(v);
            case 3:
                return new EmptyLeaf(v);
            default:
                return new SuperCaller(v);
        }
    }

    static int value(Object o) {
        return (o instanceof Plain p) ? p.v : ((Fin) o).v;
    }

    static PhantomReference<Object> phantom(int kind, ReferenceQueue<Object> q) {
        Object o = alloc(kind, 7);
        sink += value(o);
        return new PhantomReference<>(o, q);
    }

    // How many collections until the phantom of a fresh `kind` object arrives.
    static String gcs(int kind) {
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        PhantomReference<Object> p = phantom(kind, q);
        try {
            for (int gc = 1; gc <= 8; gc++) {
                System.gc();
                Reference<?> r;
                try {
                    r = q.remove(800);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    r = null;
                }
                if (r == p) {
                    return gc == 1 ? "1" : "2+";
                }
                System.runFinalization();
            }
            return "never";
        } finally {
            Reference.reachabilityFence(p);
        }
    }

    static void pass(String label) {
        StringBuilder sb = new StringBuilder(label);
        for (int k = 0; k < KINDS.length; k++) {
            sb.append(' ').append(KINDS[k]).append('=').append(gcs(k));
        }
        System.out.println(sb);
    }

    static long warm(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            // Plain, EmptySub and EmptyLeaf only: none is finalizable on HotSpot.
            int kind = (i % 3 == 0) ? 0 : (i % 3 == 1) ? 2 : 3;
            h = h * 31 + value(alloc(kind, i));
        }
        return h;
    }

    public static void main(String[] args) {
        pass("cold");
        System.out.println("warm h=" + warm(60_000));
        pass("hot");
        for (int round = 0; round < 400 && RAN.get() == 0; round++) {
            System.gc();
            System.runFinalization();
            try {
                Thread.sleep(5);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        System.out.println("fin ran=" + (RAN.get() > 0) + " sink=" + sink);
    }
}
