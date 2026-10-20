// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane fuzz8 (free choice): one OSR-compiled loop that
// mixes a VarHandle compareAndSet on an instance field (half of the CASes
// fail by construction), a VarHandle getAndAdd on a long[] element, a
// megamorphic interface call into DEFAULT methods (one overridden, one
// calling another default through `I.super`), a CHA-inlined virtual call, and
// mid-loop arrivals of never-loaded classes: a sixth interface implementation
// and a second subclass of the CHA class. The arrivals deopt the OSR frame
// while the CAS witness value and the long accumulator are live in registers.
//
// Risk targeted: OSR entry state with live long locals and a VarHandle CAS
// result feeding a branch, the megamorphic default-method stub
// (`CRATONVM_JIT_MEGA_STUB_SERVICE_IDENTITY`) and its splice of defaults,
// class-hierarchy dependency withdrawal at define (`invalidate_for_new_subtype`)
// while an OSR activation runs, and the deopt resume of that activation.
// Every value is a pure function of the loop: `ref` recomputes the score sum
// and the CHA sum with plain arithmetic, and the CAS and array totals have
// closed forms.
//
// Run:
//   javac -d out R21Fz8OsrCasMega1.java
//   java -Xmx256m -cp out R21Fz8OsrCasMega1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz8OsrCasMega1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz8OsrCasMega1
//   cratonvm --nojit -Xmx256m -cp out R21Fz8OsrCasMega1
// HotSpot expectation (by reading; not yet run):
//   run1 casOk=150000 casFail=150000 x=300000 arr=44999850000 scoreMatch=true chaMatch=true hits=575000 live=6 subs=2
//   run2 casOk=150000 casFail=150000 x=300000 arr=44999850000 scoreMatch=true chaMatch=true hits=575000 live=6 subs=2
// (x, arr and hits are reset before each run. hits counts score() bodies:
// 2 per call for 150001 calls before the arrival, then 2 per call for the
// 124999 non-S5 calls and 1 per call for the 25000 S5 calls.)

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class R21Fz8OsrCasMega1 {
    static final VarHandle X;
    static final VarHandle LA;
    static final VarHandle HITS;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            X = l.findVarHandle(Cell.class, "x", int.class);
            LA = MethodHandles.arrayElementVarHandle(long[].class);
            HITS = l.findStaticVarHandle(R21Fz8OsrCasMega1.class, "hits", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static volatile int hits;

    static final class Cell {
        volatile int x;
    }

    interface Shape {
        int base();

        default int score(int k) {
            HITS.getAndAdd(1);
            return base() * 3 + k;
        }

        default int twice(int k) {
            return score(k) + score(k + 1);
        }
    }

    interface Sub extends Shape {
        @Override
        default int score(int k) {
            return Shape.super.score(k) ^ 1;
        }
    }

    static final class S0 implements Shape {
        public int base() {
            return 1;
        }
    }

    static final class S1 implements Shape {
        public int base() {
            return 2;
        }
    }

    static final class S2 implements Sub {
        public int base() {
            return 3;
        }
    }

    static final class S3 implements Shape {
        public int base() {
            return 4;
        }

        @Override
        public int score(int k) {
            HITS.getAndAdd(1);
            return base() * 5 - k;
        }
    }

    static final class S4 implements Sub {
        public int base() {
            return 5;
        }
    }

    // Never loaded before the first arrival.
    static final class S5 implements Shape {
        public int base() {
            return 6;
        }

        @Override
        public int twice(int k) {
            return score(k) * 2;
        }
    }

    static class Cha {
        int f(int i) {
            return i & 15;
        }
    }

    // Never loaded before the first arrival.
    static class ChaLate extends Cha {
        @Override
        int f(int i) {
            return (i & 15) + 100;
        }
    }

    static final Shape[] SHAPES = new Shape[6];
    static int live;
    static int subs;
    static Cha cha;

    static void reset() {
        SHAPES[0] = new S0();
        SHAPES[1] = new S1();
        SHAPES[2] = new S2();
        SHAPES[3] = new S3();
        SHAPES[4] = new S4();
        live = 5;
        subs = 1;
        cha = new Cha();
        hits = 0;
    }

    static void arrive() {
        SHAPES[5] = new S5();
        live = 6;
        cha = new ChaLate();
        subs = 2;
    }

    // Plain-arithmetic mirror of Shape/Sub/S3/S5 score/twice.
    static int scoreRef(int t, int k) {
        int b = t + 1;
        switch (t) {
            case 2:
            case 4:
                return (b * 3 + k) ^ 1;
            case 3:
                return b * 5 - k;
            default:
                return b * 3 + k;
        }
    }

    static int twiceRef(int t, int k) {
        if (t == 5) {
            return scoreRef(t, k) * 2;
        }
        return scoreRef(t, k) + scoreRef(t, k + 1);
    }

    static int hitsRef(int t) {
        return t == 5 ? 1 : 2;
    }

    static long[] ref(int n) {
        long score = 0;
        long chaSum = 0;
        long hitSum = 0;
        int lv = 5;
        boolean late = false;
        for (int i = 0; i < n; i++) {
            int t = i % lv;
            score += twiceRef(t, i & 31);
            hitSum += hitsRef(t);
            chaSum += late ? (i & 15) + 100 : (i & 15);
            if (i == n / 2) {
                lv = 6;
                late = true;
            }
        }
        return new long[] {score, chaSum, hitSum};
    }

    static String run(int n) {
        reset();
        Cell c = new Cell();
        long[] arr = new long[8];
        long score = 0;
        long chaSum = 0;
        int ok = 0;
        int fail = 0;
        for (int i = 0; i < n; i++) {
            int old = (int) X.getVolatile(c);
            // Fails by construction on odd i: the expected value is off by one.
            if (X.compareAndSet(c, old + (i & 1), old + (i % 5))) {
                ok++;
            } else {
                fail++;
            }
            LA.getAndAdd(arr, i & 7, (long) i);
            Shape s = SHAPES[i % live];
            score += s.twice(i & 31);
            chaSum += cha.f(i);
            if (i == n / 2) {
                arrive();
            }
        }
        long total = 0;
        for (long v : arr) {
            total += v;
        }
        long[] r = ref(n);
        return "casOk=" + ok + " casFail=" + fail + " x=" + c.x + " arr=" + total
                + " scoreMatch=" + (score == r[0]) + " chaMatch=" + (chaSum == r[1])
                + " hits=" + (hits == r[2] ? Integer.toString(hits) : "MISMATCH " + hits + "!=" + r[2])
                + " live=" + live + " subs=" + subs;
    }

    public static void main(String[] args) {
        System.out.println("run1 " + run(300000));
        System.out.println("run2 " + run(300000));
    }
}
