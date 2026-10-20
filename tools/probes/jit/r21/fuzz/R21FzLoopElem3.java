// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: LOOPS21-1 write-back and kinds
// (`CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE`; the conditional write-back
// diamond `if (v != init) a[k] = v`, the `ElemWalk` through body memory phis,
// the int/long-only kind screen).
//
// Risk targeted:
//   * a store under a branch inside the loop (the walk's body-join memory
//     phi): the exit value must be the join's value on the LAST trip;
//   * a loop whose net change is zero (`+x` then `-x`): the write-back is
//     skipped; the element must still read its pre-loop value, including when
//     another reference wrote that element just BEFORE the loop;
//   * long elements whose low 32 bits return to the initial value while the
//     high bits differ (the write-back compare must be the full 64-bit LCmp);
//   * narrow arrays (byte / short / char / boolean) are out of scope and must
//     keep their per-store truncation (wrapping counters);
//   * int overflow wrap of the promoted value.
// Rows cold, hot, after a deopt trigger (a rare exception mid-loop of a
// sibling kernel and a late class).
//
// Run:
//   javac -d out R21FzLoopElem3.java
//   java -Xmx256m -cp out R21FzLoopElem3                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopElem3
//   CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1 cratonvm -Xmx256m -cp out R21FzLoopElem3
//   cratonvm --compatible -Xmx256m -cp out R21FzLoopElem3
//   cratonvm --nojit -Xmx256m -cp out R21FzLoopElem3
// HotSpot expectation: every line identical on every arm.

public class R21FzLoopElem3 {
    static final int WARM = 50_000;

    static int branchStore(int[] a, int k, int n, int mask) {
        for (int i = 0; i < n; i++) {
            if ((i & mask) == 0) {
                a[k] += i;
            } else {
                a[k] -= 1;
            }
        }
        return n;
    }

    static int oneSided(int[] a, int k, int n, int mask) {
        for (int i = 0; i < n; i++) {
            if ((i & mask) == mask) {
                a[k] = i * 7;
            }
        }
        return n;
    }

    static int netZero(int[] a, int k, int n, int x) {
        for (int i = 0; i < n; i++) {
            a[k] += x + i;
            a[k] -= x + i;
        }
        return n;
    }

    static int writeThenLoop(int[] alias, int[] a, int k, int n) {
        alias[k] = 1234;
        for (int i = 0; i < n; i++) {
            a[k] += i;
            a[k] -= i;
        }
        return a[k];
    }

    static long highBits(long[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += 1L << 32;
        }
        return a[k];
    }

    static int wrap(int[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += 0x3FFF_FFFF;
        }
        return a[k];
    }

    static int narrowByte(byte[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += 7;
        }
        return a[k];
    }

    static int narrowShort(short[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += 4099;
        }
        return a[k];
    }

    static int narrowChar(char[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] -= 3;
        }
        return a[k];
    }

    static int narrowBool(boolean[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] ^= true;
        }
        return a[k] ? 1 : 0;
    }

    /** A sibling kernel that throws mid-loop on a rare input (the deopt trigger). */
    static int throwing(int[] a, int k, int n, int bad) {
        for (int i = 0; i < n; i++) {
            a[k] += 100 / (i - bad);
        }
        return a[k];
    }

    static String arr(int[] a) {
        return java.util.Arrays.toString(a);
    }

    static void rows(String tag) {
        int[] a = {10, 20, 30};
        branchStore(a, 1, 37, 3);
        System.out.println("row " + tag + " branchStore " + arr(a));
        a = new int[] {10, 20, 30};
        oneSided(a, 2, 37, 7);
        System.out.println("row " + tag + " oneSided-hit " + arr(a));
        a = new int[] {10, 20, 30};
        oneSided(a, 2, 5, 7);
        System.out.println("row " + tag + " oneSided-miss " + arr(a));
        a = new int[] {10, 20, 30};
        netZero(a, 0, 50, 99);
        System.out.println("row " + tag + " netZero " + arr(a));
        a = new int[] {10, 20, 30};
        int w = writeThenLoop(a, a, 0, 20);
        System.out.println("row " + tag + " writeThenLoop " + w + " " + arr(a));
        long[] l = {5, 6};
        long hb = highBits(l, 1, 4);
        System.out.println("row " + tag + " highBits " + hb + " " + java.util.Arrays.toString(l));
        l = new long[] {5, 6};
        hb = highBits(l, 0, 0);
        System.out.println("row " + tag + " highBits0 " + hb + " " + java.util.Arrays.toString(l));
        a = new int[] {1};
        System.out.println("row " + tag + " wrap " + wrap(a, 0, 13) + " " + arr(a));
        byte[] by = {0, 120};
        System.out.println("row " + tag + " byte " + narrowByte(by, 1, 41) + " " + java.util.Arrays.toString(by));
        short[] sh = {0, 32000};
        System.out.println("row " + tag + " short " + narrowShort(sh, 1, 23) + " " + java.util.Arrays.toString(sh));
        char[] ch = {'a', 'b'};
        System.out.println("row " + tag + " char " + narrowChar(ch, 0, 51) + " " + (int) ch[0]);
        boolean[] bo = {false, true};
        System.out.println("row " + tag + " bool " + narrowBool(bo, 1, 7) + " " + java.util.Arrays.toString(bo));
        a = new int[] {0};
        try {
            throwing(a, 0, 20, 11);
            System.out.println("row " + tag + " throwing ok " + arr(a));
        } catch (ArithmeticException e) {
            System.out.println("row " + tag + " throwing " + e.getMessage() + " " + arr(a));
        }
    }

    static final class Late {
        static int bad() {
            return 3;
        }
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        int[] a = new int[4];
        long[] l = new long[4];
        byte[] by = new byte[4];
        short[] sh = new short[4];
        char[] ch = new char[4];
        boolean[] bo = new boolean[4];
        for (int r = 0; r < WARM; r++) {
            int k = r & 3;
            branchStore(a, k, 9, r & 3);
            oneSided(a, k, 9, (r >> 2) & 7);
            netZero(a, k, 6, r);
            h += writeThenLoop(a, a, k, 4);
            h += highBits(l, k, 2);
            h += wrap(a, k, 3);
            h += narrowByte(by, k, 5);
            h += narrowShort(sh, k, 5);
            h += narrowChar(ch, k, 5);
            h += narrowBool(bo, k, 3);
            h += throwing(a, k, 6, -1);
            h = h * 31 + a[k];
        }
        System.out.println("warm hash " + h + " " + arr(a) + " " + java.util.Arrays.toString(l));
        rows("hot");
        int[] t = {0};
        try {
            throwing(t, 0, 10, Late.bad());
        } catch (ArithmeticException e) {
            System.out.println("late " + e.getMessage() + " " + arr(t));
        }
        rows("after");
    }
}
