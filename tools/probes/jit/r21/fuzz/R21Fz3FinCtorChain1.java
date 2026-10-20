// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13, lane fuzz3: finalizer registration of objects whose
// constructor CHAIN throws, beyond R21FinalizeCtorThrows (one level, one class
// per kind). Waves 8-12 made registration follow HotSpot's
// RegisterFinalizersAtInit rule (registered when Object.<init> returns) by
// registering at `new` and WITHDRAWING the row when the unwind proves the
// object never reached Object.<init> (`exception_dispatch.rs`
// `unconstructed_mark_of_ctor` / `unconstructed_mark_step`, the doors, and
// wave 11's `finalizer_bars_compiling`: a finalizable ctor that can throw
// before super stays interpreted).
//
// Risk targeted:
//  * the mark is CARRIED through chain frames (an `<init>` of the same object
//    at `invokespecial`) and the CREATOR withdraws. Kinds whose creator is
//    itself an `<init>` frame of ANOTHER finalizable object that already
//    returned from Object.<init> (`holder`, `holderCatch`): the holder must stay
//    registered, only the inner object is withdrawn;
//  * a throw two levels up the chain (`midBefore`: Sub(v){super(v);} over
//    Mid(v){super(check(v));}) vs after (`midAfter`, `subAfter`);
//  * `this(..)` delegation that completes then throws (`thisThenThrow`: N) vs
//    `this(check(v))` (`thisArgThrows`: 0);
//  * a finalizable object fully built inside the super() ARGUMENT of a ctor that
//    then throws before super (`beforeNested` 0, its `nestedNormal` N);
//  * an object that escapes (stored in a static array) and then throws after
//    super: not finalized while reachable, N after the array is cleared;
//  * creators: bytecode `new`, Constructor.newInstance, MethodHandle
//    findConstructor, a constructor method reference (`SubMB::new`, the
//    lambda proxy is the creator), and a JIT-hot loop (OSR) whose maker call
//    site goes bimorphic halfway through (a never-loaded Maker class is loaded
//    mid-loop: deopt / not-entrant while constructions are in flight).
//
// Counting technique (mirrors R21FinalizeCtorThrows): settle with
// System.gc()+runFinalization until the finalized total is still for 10
// rounds (at least 20, at most 400), reset, construct, settle, print.
//
// Run:
//   javac -d out R21Fz3FinCtorChain1.java
//   java -Xmx256m -cp out R21Fz3FinCtorChain1                       # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz3FinCtorChain1                   # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz3FinCtorChain1
//   cratonvm --nojit -Xmx256m -cp out R21Fz3FinCtorChain1
//   cratonvm -XX:+UseG1GC ... / -XX:+UseZGC ...
//   CRATONVM_FINALIZER_CTOR_STAYS_INTERPRETED=0 cratonvm ...    # compiled ctors (the residual arm)
// HotSpot expectation: every line identical on every arm. By the rule, every
// row reads: midBefore=0 midAfter=N subAfter=N thisThenThrow=N thisArgThrows=0
// holder=N holderInner=0 holderCatch=N holderCatchInner=0 escapeKept=0
// escapeFreed=N normal=N beforeNested=0 nestedNormal=N (N = 4000, hot rows
// 12000), but HotSpot's run is the authority.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.IntFunction;

public class R21Fz3FinCtorChain1 {
    static final int N = 4000;
    static final int HOT = 12_000;
    static final String[] NAMES = {
        "midBefore", "midAfter", "subAfter", "thisThenThrow", "thisArgThrows", "holder", "holderInner",
        "holderCatch", "holderCatchInner", "escape", "normal", "beforeNested", "nestedNormal"
    };
    /** The kinds a route constructs directly; the others are built as side effects. */
    static final int[] TOP = {0, 1, 2, 3, 4, 5, 7, 9, 10, 11};
    static final AtomicInteger[] FIN = new AtomicInteger[NAMES.length];
    static final AtomicInteger TOTAL = new AtomicInteger();
    static final Object[] KEEP = new Object[HOT];
    static int holderCaught;

    static {
        for (int i = 0; i < FIN.length; i++) {
            FIN[i] = new AtomicInteger();
        }
    }

    static int check(int v) {
        if (v >= 0) {
            throw new IllegalArgumentException("check " + v);
        }
        return v;
    }

    abstract static class Fin {
        Fin(int v) {
        }

        abstract int kind();

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FIN[kind()].incrementAndGet();
            TOTAL.incrementAndGet();
        }
    }

    abstract static class MidB extends Fin {
        MidB(int v) {
            super(check(v));
        }
    }

    static class SubMB extends MidB {
        SubMB(int v) {
            super(v + 1);
        }

        int kind() {
            return 0;
        }
    }

    abstract static class MidA extends Fin {
        MidA(int v) {
            super(v);
            check(v);
        }
    }

    static class SubMA extends MidA {
        SubMA(int v) {
            super(v);
        }

        int kind() {
            return 1;
        }
    }

    static class Norm extends Fin {
        Norm(int v) {
            super(v);
        }

        int kind() {
            return 10;
        }
    }

    static class SA extends Norm {
        SA(int v) {
            super(v);
            check(v);
        }

        @Override
        int kind() {
            return 2;
        }
    }

    static class TT extends Fin {
        TT() {
            super(0);
        }

        TT(int v) {
            this();
            check(v);
        }

        int kind() {
            return 3;
        }
    }

    static class TA extends Fin {
        TA(int a, int b) {
            super(a);
        }

        TA(int v) {
            this(check(v), v);
        }

        int kind() {
            return 4;
        }
    }

    static class Bf extends Fin {
        Bf(int v) {
            super(check(v));
        }

        int kind() {
            return 6;
        }
    }

    static class Holder extends Fin {
        Holder(int v) {
            super(v);
            new Bf(v);
        }

        int kind() {
            return 5;
        }
    }

    static class Bf2 extends Fin {
        Bf2(int v) {
            super(check(v));
        }

        int kind() {
            return 8;
        }
    }

    static class HolderC extends Fin {
        HolderC(int v) {
            super(v);
            try {
                new Bf2(v);
            } catch (IllegalArgumentException e) {
                holderCaught++;
            }
            throw new IllegalStateException("holderC " + v);
        }

        int kind() {
            return 7;
        }
    }

    static class Esc extends Fin {
        Esc(int v) {
            super(v);
            KEEP[v] = this;
            check(v);
        }

        int kind() {
            return 9;
        }
    }

    static class NN extends Fin {
        NN(int v) {
            super(v);
        }

        int kind() {
            return 12;
        }
    }

    static class BN extends Fin {
        BN(int v) {
            super(mk(v));
        }

        static int mk(int v) {
            new NN(v);
            return check(v);
        }

        int kind() {
            return 11;
        }
    }

    static Object make(int kind, int r) {
        switch (kind) {
            case 0: return new SubMB(r);
            case 1: return new SubMA(r);
            case 2: return new SA(r);
            case 3: return new TT(r);
            case 4: return new TA(r);
            case 5: return new Holder(r);
            case 7: return new HolderC(r);
            case 9: return new Esc(r);
            case 10: return new Norm(r);
            default: return new BN(r);
        }
    }

    interface Maker {
        Object make(int kind, int r) throws Throwable;
    }

    static final class MakerA implements Maker {
        public Object make(int kind, int r) {
            return R21Fz3FinCtorChain1.make(kind, r);
        }
    }

    /** Loaded first in the middle of the first hot loop: the deopt trigger. */
    static final class MakerLate implements Maker {
        public Object make(int kind, int r) {
            Object o = R21Fz3FinCtorChain1.make(kind, r);
            return o;
        }
    }

    static Maker maker = new MakerA();
    static boolean lateLoaded;

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
            int now = TOTAL.get();
            if (now == last) {
                still++;
            } else {
                still = 0;
                last = now;
            }
            if (i >= 20 && still >= 10) {
                return;
            }
        }
    }

    static boolean expectThrow(int kind) {
        return kind != 10;
    }

    /** Runs one route over every top kind and prints one row. */
    static void row(String route, int n, Maker m) throws Throwable {
        Arrays.fill(KEEP, null);
        settle();
        for (AtomicInteger c : FIN) {
            c.set(0);
        }
        holderCaught = 0;
        StringBuilder thrown = new StringBuilder();
        for (int kind : TOP) {
            int t = 0;
            for (int r = 0; r < n; r++) {
                try {
                    m.make(kind, r);
                } catch (IllegalArgumentException | IllegalStateException | InvocationTargetException e) {
                    t++;
                }
            }
            if (t != (expectThrow(kind) ? n : 0)) {
                thrown.append(' ').append(NAMES[kind]).append("Thrown=").append(t);
            }
        }
        settle();
        StringBuilder sb = new StringBuilder("row ").append(route).append(':');
        for (int k = 0; k < NAMES.length; k++) {
            sb.append(' ').append(k == 9 ? "escapeKept" : NAMES[k]).append('=').append(FIN[k].get());
        }
        Arrays.fill(KEEP, null);
        settle();
        sb.append(" escapeFreed=").append(FIN[9].get());
        sb.append(" holderCaught=").append(holderCaught);
        System.out.println(sb.append(thrown));
    }

    /** The JIT-hot route: one method per call, its own loop and handler (OSR on the first call). */
    static int hot(int kind, int n) {
        int t = 0;
        Maker m = maker;
        for (int r = 0; r < n; r++) {
            if (r == n / 2 && !lateLoaded) {
                lateLoaded = true;
                maker = new MakerLate();
                m = maker;
            } else if (r == n / 2) {
                m = (kind & 1) == 0 ? new MakerA() : maker;
            }
            try {
                m.make(kind, r);
            } catch (IllegalArgumentException | IllegalStateException e) {
                t++;
            } catch (Throwable e) {
                throw new AssertionError(e);
            }
        }
        return t;
    }

    public static void main(String[] args) throws Throwable {
        Maker byNew = (k, r) -> make(k, r);
        row("new", N, byNew);

        Constructor<?>[] ctors = new Constructor<?>[NAMES.length];
        Class<?>[] cls = new Class<?>[NAMES.length];
        cls[0] = SubMB.class;
        cls[1] = SubMA.class;
        cls[2] = SA.class;
        cls[3] = TT.class;
        cls[4] = TA.class;
        cls[5] = Holder.class;
        cls[7] = HolderC.class;
        cls[9] = Esc.class;
        cls[10] = Norm.class;
        cls[11] = BN.class;
        for (int k : TOP) {
            ctors[k] = cls[k].getDeclaredConstructor(int.class);
        }
        row("ctor", N, (k, r) -> ctors[k].newInstance(r));

        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle[] mhs = new MethodHandle[NAMES.length];
        for (int k : TOP) {
            mhs[k] = lookup.findConstructor(cls[k], MethodType.methodType(void.class, int.class))
                    .asType(MethodType.methodType(Object.class, int.class));
        }
        row("mh", N, (k, r) -> {
            Object o = (Object) mhs[k].invokeExact(r);
            return o;
        });

        @SuppressWarnings("unchecked")
        IntFunction<Object>[] refs = new IntFunction[NAMES.length];
        refs[0] = SubMB::new;
        refs[1] = SubMA::new;
        refs[2] = SA::new;
        refs[3] = TT::new;
        refs[4] = TA::new;
        refs[5] = Holder::new;
        refs[7] = HolderC::new;
        refs[9] = Esc::new;
        refs[10] = Norm::new;
        refs[11] = BN::new;
        row("ref", N, (k, r) -> refs[k].apply(r));

        // Hot: OSR on the first call of hot(..) per kind, full compile later; the
        // maker site flips at n/2.
        for (int pass = 0; pass < 2; pass++) {
            Arrays.fill(KEEP, null);
            settle();
            for (AtomicInteger c : FIN) {
                c.set(0);
            }
            holderCaught = 0;
            StringBuilder thrown = new StringBuilder();
            for (int kind : TOP) {
                int t = hot(kind, HOT);
                if (t != (expectThrow(kind) ? HOT : 0)) {
                    thrown.append(' ').append(NAMES[kind]).append("Thrown=").append(t);
                }
            }
            settle();
            StringBuilder sb = new StringBuilder("row hot").append(pass).append(':');
            for (int k = 0; k < NAMES.length; k++) {
                sb.append(' ').append(k == 9 ? "escapeKept" : NAMES[k]).append('=').append(FIN[k].get());
            }
            Arrays.fill(KEEP, null);
            settle();
            sb.append(" escapeFreed=").append(FIN[9].get());
            sb.append(" holderCaught=").append(holderCaught);
            System.out.println(sb.append(thrown));
        }
        // After everything is warm: the cold routes again through compiled callers.
        row("new-again", N, byNew);
        row("ctor-again", N, (k, r) -> ctors[k].newInstance(r));
        System.out.println("done");
    }
}
