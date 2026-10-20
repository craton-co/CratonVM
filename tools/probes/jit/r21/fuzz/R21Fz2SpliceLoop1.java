// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: cell promotion inside SPLICED callees
// (`CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED`, opt-in;
// `loop_field_forward::base_null_checked_before` (the splice's dominating
// null guard proves the base) and `promotion_header_frame` (the chain
// snapshot serves the exit write-back)).
//
// Risk targeted: `acc.addAll(a)` spliced into a caller's loop keeps
// `acc.sum` in a register for the callee's loop and stores it at the exit.
// Every way out of that loop must store first: an AIOOBE mid-loop caught by
// the CALLER (the partial sum must be in the field), an ArithmeticException
// caught in the callee, an early return, a break. The base must really be
// non-null (a null receiver throws NPE at the call, before any store; a null
// array throws at `a.length`). The base may ALIAS another argument
// (`acc.addFrom(acc)` reads `other.sum` each trip: the promoted cell must not
// hide the writes from the read). A long cell; two promoted fields of one
// object; a callee spliced at two call sites of one caller (two chain
// snapshots with one header bci: `promotion_header_frame` must refuse, not
// pick one); a callee whose loop calls a method that reads `this.sum`.
// Cold, hot (50k caller calls), a late Acc subclass (the deopt trigger),
// again.
//
// Run:
//   javac -d out R21Fz2SpliceLoop1.java
//   java -Xmx256m -cp out R21Fz2SpliceLoop1                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2SpliceLoop1
//   CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1 cratonvm -Xmx256m -cp out R21Fz2SpliceLoop1
//   CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1 CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 cratonvm ...
//   ... + CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1; CRATONVM_DBG_LOOP_FIELD_FORWARD=1 (stderr: refusals)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz2SpliceLoop1 {
    static final int WARM = 60_000;

    static class Acc {
        int sum;
        int count;
        long lsum;

        void addAll(int[] a) {
            for (int i = 0; i < a.length; i++) {
                sum += a[i];
            }
        }

        void addRange(int[] a, int from, int to) {
            for (int i = from; i < to; i++) {
                sum += a[i];
                count++;
            }
        }

        void addDiv(int[] a, int k) {
            try {
                for (int i = 0; i < a.length; i++) {
                    sum += 1000 / (a[i] - k);
                }
            } catch (ArithmeticException e) {
                sum = -sum;
            }
        }

        int addUntil(int[] a, int stop) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] == stop) {
                    return i;
                }
                sum += a[i];
            }
            return -1;
        }

        void addFrom(Acc other, int n) {
            for (int i = 0; i < n; i++) {
                sum += other.sum + 1;
            }
        }

        void addLong(long[] a) {
            for (int i = 0; i < a.length; i++) {
                lsum += a[i];
            }
        }

        int peek() {
            return sum & 7;
        }

        void addPeek(int[] a) {
            for (int i = 0; i < a.length; i++) {
                sum += a[i] + peek();
            }
        }
    }

    static class LateAcc extends Acc {
        @Override
        int peek() {
            return -(sum & 3);
        }
    }

    static long d0(Acc acc, int[] a) {
        long t = 0;
        for (int k = 0; k < 4; k++) {
            acc.addAll(a);
            t += acc.sum;
        }
        return t;
    }

    static long d1(Acc acc, int[] a, int to) {
        long t = 0;
        for (int k = 0; k < 3; k++) {
            try {
                acc.addRange(a, 0, to);
            } catch (ArrayIndexOutOfBoundsException e) {
                t += 1_000_000;
            }
            t += acc.sum * 10L + acc.count;
        }
        return t;
    }

    static long d2(Acc acc, int[] a, int k) {
        acc.addDiv(a, k);
        return acc.sum;
    }

    static long d3(Acc acc, int[] a, int stop) {
        int at = acc.addUntil(a, stop);
        return acc.sum * 100L + at;
    }

    static long d4(Acc acc, Acc other, int n) {
        acc.addFrom(other, n);
        return acc.sum;
    }

    static long d5(Acc acc, long[] a) {
        acc.addLong(a);
        acc.addLong(a);
        return acc.lsum;
    }

    static long d6(Acc acc, int[] a) {
        acc.addAll(a);
        long mid = acc.sum;
        acc.addAll(a);
        return mid * 1000 + acc.sum;
    }

    static long d7(Acc acc, int[] a) {
        acc.addPeek(a);
        return acc.sum;
    }

    static String safe(java.util.function.LongSupplier s) {
        try {
            return Long.toString(s.getAsLong());
        } catch (NullPointerException e) {
            return "NPE";
        } catch (ArrayIndexOutOfBoundsException e) {
            return "AIOOBE(" + e.getMessage() + ")";
        }
    }

    static String row(java.util.function.Supplier<Acc> mk, int[] a, long[] la) {
        Acc shared = mk.get();
        Acc nul = null;
        return safe(() -> d0(mk.get(), a)) + " " + safe(() -> d1(mk.get(), a, a.length)) + " "
                + safe(() -> d1(mk.get(), a, a.length + 2)) + " " + safe(() -> d2(mk.get(), a, 3)) + " "
                + safe(() -> d2(mk.get(), a, 100)) + " " + safe(() -> d3(mk.get(), a, 5)) + " "
                + safe(() -> d3(mk.get(), a, -1)) + " " + safe(() -> d4(shared, shared, 5)) + " "
                + safe(() -> d4(mk.get(), shared, 5)) + " " + safe(() -> d5(mk.get(), la)) + " "
                + safe(() -> d6(mk.get(), a)) + " " + safe(() -> d7(mk.get(), a)) + " | "
                + safe(() -> d0(nul, a)) + " " + safe(() -> d0(mk.get(), null)) + " " + safe(() -> d1(mk.get(), null, 3));
    }

    public static void main(String[] args) {
        int[] a = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12};
        long[] la = {Long.MAX_VALUE / 4, -3, 1L << 40, 7};
        java.util.function.Supplier<Acc> plain = Acc::new;
        System.out.println("row cold | " + row(plain, a, la));
        long h = 0;
        Acc self = new Acc();
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + d0(new Acc(), a) + d1(new Acc(), a, a.length) + d2(new Acc(), a, 100) + d3(new Acc(), a, 99);
            h = h * 31 + d4(self, self, 2) + d5(new Acc(), la) + d6(new Acc(), a) + d7(new Acc(), a);
            if ((r & 1023) == 0) {
                self = new Acc();
            }
        }
        System.out.println("warm hash " + h);
        System.out.println("row hot | " + row(plain, a, la));
        // Deopt trigger: the exceptional exits for the first time hot, and a
        // receiver subclass at every call site.
        System.out.println("rare | " + safe(() -> d1(new Acc(), a, a.length + 5)) + " " + d2(new Acc(), a, 4) + " "
                + d3(new Acc(), a, 7));
        java.util.function.Supplier<Acc> late = LateAcc::new;
        System.out.println("row late | " + row(late, a, la));
        System.out.println("row after | " + row(plain, a, la));
        int[] big = new int[1000];
        for (int i = 0; i < big.length; i++) {
            big[i] = i % 17 - 8;
        }
        System.out.println("row big | " + row(plain, big, la));
    }
}
