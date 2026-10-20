// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: LOOPS21-1 aliasing
// (`CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE`,
// `loop_field_forward::promote_elem_loop`, the census "one cell per kind").
//
// Risk targeted: an element has no field identity -- `b[j]` IS `a[k]` when
// `b == a && j == k` at run time. The census must refuse a loop with a second
// access of the cell's kind (any base, any index) and may take one int and
// one long cell together (different arrays by kind). Rows:
//   * two references to the SAME int[] with equal indices (must see each
//     other's updates every iteration);
//   * the same array, two DIFFERENT index nodes that are equal at run time;
//   * a read of another element `a[k2]` of the cell's array in the loop;
//   * one int cell and one long cell in one loop (both may be promoted);
//   * an int[] cell plus a float[] access (another kind: refused);
//   * the cell array passed through an Object field and cast back;
//   * a self-feedback `a[k] = a[k] * 3 + a.length`.
// Same rows cold, hot, and after a deopt trigger (a second implementation of
// an interface the kernel's caller dispatches on, loaded late).
//
// Run:
//   javac -d out R21FzLoopElem2.java
//   java -Xmx256m -cp out R21FzLoopElem2                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopElem2
//   CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1 cratonvm -Xmx256m -cp out R21FzLoopElem2
//   cratonvm --compatible -Xmx256m -cp out R21FzLoopElem2
//   cratonvm --nojit -Xmx256m -cp out R21FzLoopElem2
// HotSpot expectation: every line identical on every arm.

public class R21FzLoopElem2 {
    static final int WARM = 40_000;

    static int twoRefs(int[] a, int[] b, int k, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += 1;
            b[j] = b[j] * 2 + i;
        }
        return a[0];
    }

    static int sameArrayTwoIdx(int[] a, int k, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += i;
            a[j] ^= i;
        }
        return a[0];
    }

    static int readOther(int[] a, int k, int k2, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            a[k] += 3;
            s += a[k2];
        }
        return s;
    }

    static long intAndLong(int[] a, long[] l, int k, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += i;
            l[j] += (long) a.length * i - 1;
        }
        return l[j] + a[k];
    }

    static int intAndFloat(int[] a, float[] f, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += 2;
            f[k] += 0.5f;
        }
        return a[k] + (int) f[k];
    }

    static Object box;

    static int viaObject(int k, int n) {
        int[] a = (int[]) box;
        for (int i = 0; i < n; i++) {
            a[k] += i * i;
        }
        return a[k];
    }

    static int feedback(int[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] = a[k] * 3 + a.length;
        }
        return a[k];
    }

    interface Feeder {
        int n(int r);
    }

    static final class Fixed implements Feeder {
        public int n(int r) {
            return 9;
        }
    }

    static final class Late implements Feeder {
        public int n(int r) {
            return r & 15;
        }
    }

    static String arr(int[] a) {
        return java.util.Arrays.toString(a);
    }

    static void rows(String tag, Feeder f) {
        int n = f.n(5);
        int[] a = {1, 2, 3, 4};
        int r1 = twoRefs(a, a, 1, 1, n);
        System.out.println("row " + tag + " twoRefs-alias " + r1 + " " + arr(a));
        a = new int[] {1, 2, 3, 4};
        int[] b = {1, 2, 3, 4};
        int r2 = twoRefs(a, b, 1, 1, n);
        System.out.println("row " + tag + " twoRefs-distinct " + r2 + " " + arr(a) + " " + arr(b));
        a = new int[] {5, 6, 7, 8};
        int r3 = sameArrayTwoIdx(a, 2, 2, n);
        System.out.println("row " + tag + " sameIdx " + r3 + " " + arr(a));
        a = new int[] {5, 6, 7, 8};
        int r4 = sameArrayTwoIdx(a, 2, 3, n);
        System.out.println("row " + tag + " otherIdx " + r4 + " " + arr(a));
        a = new int[] {0, 10, 20, 30};
        int r5 = readOther(a, 1, 1, n);
        System.out.println("row " + tag + " readSame " + r5 + " " + arr(a));
        a = new int[] {0, 10, 20, 30};
        int r6 = readOther(a, 1, 2, n);
        System.out.println("row " + tag + " readOther " + r6 + " " + arr(a));
        a = new int[] {1, 1, 1};
        long[] l = {7, 8, 9};
        long r7 = intAndLong(a, l, 0, 2, n);
        System.out.println("row " + tag + " intLong " + r7 + " " + arr(a) + " " + java.util.Arrays.toString(l));
        a = new int[] {1, 1, 1};
        float[] fl = {0f, 0f, 0f};
        int r8 = intAndFloat(a, fl, 1, n);
        System.out.println("row " + tag + " intFloat " + r8 + " " + arr(a) + " " + java.util.Arrays.toString(fl));
        box = new int[] {4, 4, 4};
        int r9 = viaObject(2, n);
        System.out.println("row " + tag + " viaObject " + r9 + " " + arr((int[]) box));
        a = new int[] {1, 2};
        int r10 = feedback(a, 1, n);
        System.out.println("row " + tag + " feedback " + r10 + " " + arr(a));
    }

    public static void main(String[] args) {
        Feeder fixed = new Fixed();
        rows("cold", fixed);
        long h = 0;
        int[] a = new int[4];
        int[] b = new int[4];
        long[] l = new long[4];
        float[] fl = new float[4];
        for (int r = 0; r < WARM; r++) {
            int n = 8 + (r & 7);
            int k = r & 3;
            h += twoRefs(a, (r & 1) == 0 ? a : b, k, (r >> 1) & 3, n);
            h += sameArrayTwoIdx(a, k, (r >> 2) & 3, n);
            h += readOther(a, k, (r >> 3) & 3, n);
            h += intAndLong(a, l, k, (r >> 1) & 3, n);
            h += intAndFloat(a, fl, k, n);
            box = a;
            h += viaObject(k, n);
            h += feedback(b, k, 3);
            h = h * 31 + a[k];
        }
        System.out.println("warm hash " + h + " " + arr(a) + " " + arr(b) + " " + java.util.Arrays.toString(l));
        rows("hot", fixed);
        Feeder late = new Late();
        rows("late", late);
        rows("after", fixed);
    }
}
