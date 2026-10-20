// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 10, lane finalize: when is an object whose constructor
// throws finalized? (page docs/internal/fixed-bugs/r21-review-w68-native-findings-FIXED-20261019.md
// item 4; residual docs/internal/fixed-bugs/r21-finalize-compiled-ctor-throw-keeps-registration-FIXED-20261019.md).
//
// HotSpot (RegisterFinalizersAtInit=true, the default) registers a finalizable
// object when its Object.<init> returns. So:
//   before  -- Before(int v) { super(check(v)); }: check throws before super()
//              runs, Object.<init> never returns -> never finalized (0);
//   after   -- After(int v) { super(v); throw ...; }: Object.<init> returned
//              -> finalized (N);
//   normal  -- Normal(int v) { super(v); } -> finalized (N).
// Routes: bytecode `new`, Constructor.newInstance (JDK 25: constructor
// MethodHandle -> Unsafe.allocateInstance + linkToSpecial <init>),
// MethodHandles.findConstructor invokeExact, and a JIT-hot loop (one method
// run HOT times, so the loop and the constructors compile).
//
// Run:
//   javac -d out R21FinalizeCtorThrows.java
//   java -Xmx256m -cp out R21FinalizeCtorThrows                       # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FinalizeCtorThrows                   # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21FinalizeCtorThrows
//   cratonvm --nojit -Xmx256m -cp out R21FinalizeCtorThrows
//   (also -XX:+UseG1GC and -XX:+UseZGC)
// Kill-switch control: CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT=0 -> every
// `before` row reads N (HOT for the hot row): the pre-fix defect.
//
// HotSpot expectation (every arm, both modes):
//   fin before new 0
//   fin after new 20000
//   fin normal new 20000
//   fin before ctor 0
//   fin after ctor 20000
//   fin normal ctor 20000
//   fin before mh 0
//   fin after mh 20000
//   fin normal mh 20000
//   fin before hot 0
//   fin after hot 50000
//   fin normal hot 50000
// CratonVM: every line must match, --nojit and default (wave 11: a
// finalizable constructor that can throw before super() stays interpreted,
// CRATONVM_FINALIZER_CTOR_STAYS_INTERPRETED; `=0` brings back a nonzero
// `fin before hot`, the compiled residual on the page above).

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.util.concurrent.atomic.AtomicInteger;

public class R21FinalizeCtorThrows {
    static final int N = 20_000;
    static final int HOT = 50_000;
    static final String[] KINDS = {"before", "after", "normal"};
    static final AtomicInteger[] FIN = {new AtomicInteger(), new AtomicInteger(), new AtomicInteger()};

    static class Fin {
        final int v;

        Fin(int v) {
            this.v = v;
        }

        int kind() {
            return -1;
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FIN[kind()].incrementAndGet();
        }
    }

    static int check(int v) {
        if (v >= 0) {
            throw new IllegalArgumentException("before super " + v);
        }
        return v;
    }

    static class Before extends Fin {
        Before(int v) {
            super(check(v));
        }

        @Override
        int kind() {
            return 0;
        }
    }

    static class After extends Fin {
        After(int v) {
            super(v);
            if (v >= 0) {
                throw new IllegalStateException("after super " + v);
            }
        }

        @Override
        int kind() {
            return 1;
        }
    }

    static class Normal extends Fin {
        Normal(int v) {
            super(v);
        }

        @Override
        int kind() {
            return 2;
        }
    }

    interface Maker {
        void make(int r) throws Throwable;
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

    /** Collect until the count has not moved for 10 rounds (at least 20, at most 400). */
    static int settle(AtomicInteger counter) {
        int last = -1;
        int still = 0;
        for (int i = 0; i < 400; i++) {
            collect();
            int now = counter.get();
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
        return counter.get();
    }

    static String row(int kind, String route, int n, Maker m) throws Throwable {
        for (AtomicInteger c : FIN) {
            settle(c);
        }
        FIN[kind].set(0);
        int thrown = 0;
        for (int r = 0; r < n; r++) {
            try {
                m.make(r);
            } catch (IllegalArgumentException | IllegalStateException e) {
                thrown++;
            } catch (InvocationTargetException e) {
                thrown++;
            }
        }
        int expectThrown = kind == 2 ? 0 : n;
        if (thrown != expectThrown) {
            throw new AssertionError(KINDS[kind] + " " + route + " thrown " + thrown);
        }
        return "fin " + KINDS[kind] + " " + route + " " + settle(FIN[kind]);
    }

    static Object make(int kind, int r) {
        switch (kind) {
            case 0:
                return new Before(r);
            case 1:
                return new After(r);
            default:
                return new Normal(r);
        }
    }

    /** The JIT-hot route: one method, HOT constructions, its own handler. */
    static int hot(int kind, int n) {
        int thrown = 0;
        for (int r = 0; r < n; r++) {
            try {
                make(kind, r);
            } catch (IllegalArgumentException | IllegalStateException e) {
                thrown++;
            }
        }
        return thrown;
    }

    public static void main(String[] args) throws Throwable {
        Class<?>[] classes = {Before.class, After.class, Normal.class};
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (int k = 0; k < 3; k++) {
            final int kind = k;
            System.out.println(row(kind, "new", N, r -> make(kind, r)));
        }
        for (int k = 0; k < 3; k++) {
            final int kind = k;
            Constructor<?> c = classes[k].getDeclaredConstructor(int.class);
            System.out.println(row(kind, "ctor", N, r -> c.newInstance(r)));
        }
        for (int k = 0; k < 3; k++) {
            final int kind = k;
            MethodHandle mh = lookup
                    .findConstructor(classes[k], MethodType.methodType(void.class, int.class))
                    .asType(MethodType.methodType(Object.class, int.class));
            System.out.println(row(kind, "mh", N, r -> {
                Object o = (Object) mh.invokeExact(r);
            }));
        }
        for (int k = 0; k < 3; k++) {
            final int kind = k;
            for (AtomicInteger c : FIN) {
                settle(c);
            }
            FIN[kind].set(0);
            int thrown = hot(kind, HOT);
            int expectThrown = kind == 2 ? 0 : HOT;
            if (thrown != expectThrown) {
                throw new AssertionError(KINDS[kind] + " hot thrown " + thrown);
            }
            System.out.println("fin " + KINDS[kind] + " hot " + settle(FIN[kind]));
        }
    }
}
