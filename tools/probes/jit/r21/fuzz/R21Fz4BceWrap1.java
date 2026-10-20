// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 15, lane fuzz4: range-check elimination on loops whose
// induction variable or index expression wraps around Integer.MAX_VALUE /
// MIN_VALUE.
//
// Risk targeted: every bounds-check-elimination planter that proves
// `0 <= f(i) < a.length` from the loop bounds assumes the induction variable
// does not wrap. Java int arithmetic wraps, so each of these loops is legal and
// its exact trip count and faulting index are part of the HotSpot answer:
//   * `inclWrap`:  `for (i = from; i <= Integer.MAX_VALUE; i++) a[i - from]`
//     -- the condition is always true, `i` wraps to MIN_VALUE and `i - from`
//     keeps counting 6, 7, ... until AIOOBE (`CRATONVM_JIT_INCLUSIVE_BCE`);
//   * `strideWrap`: stride 3 below MAX_VALUE, `i += 3` wraps negative and the
//     loop continues (`CRATONVM_JIT_IR_BCE_STRIDE3`, `CRATONVM_JIT_IR_BCE_MONOTONE`);
//   * `downWrap`:  `for (i = from; i >= Integer.MIN_VALUE; i--) a[from - i]`
//     (`CRATONVM_JIT_DOWN_COUNT_BCE`);
//   * `offWrap`:   `a[i + off]` with `off` near MAX_VALUE, the sum wraps to a
//     huge negative index (`CRATONVM_JIT_IR_BCE_VAR_OFFSET`, `..._ROOM_OFFSET`);
//   * `mulWrap`:   `a[i * 3 - base]` where `i * 3` crosses MAX_VALUE
//     (`CRATONVM_JIT_IR_BCE_RANGE`);
//   * `maskAll`:   `a[i & m]` with m = -1 and m = 0x7fffffff (a mask that does
//     not bound the index, `CRATONVM_JIT_IR_BCE_MASK_INDEX` / `..._CONST_MASK`);
//   * `clampOff1`: `a[Math.min(i, lim)]` with lim = a.length (off by one,
//     `CRATONVM_JIT_IR_BCE_CLAMP_INDEX` / `..._CLAMP_PREDICATE`);
//   * `fieldShrink`: `for (i < this.arr.length) this.arr[i]` while a virtual
//     hook replaces `this.arr` mid-loop (`CRATONVM_JIT_FIELD_ARRAY_BCE`,
//     `CRATONVM_JIT_INLINE_LENGTH_BCE`); a late Hook subclass that GROWS or
//     NULLS the array is the deopt trigger.
// Each kernel returns `trips * 1_000_003 + sum` (arrays hold i + 1, so sums are
// positive), bit-inverted when the loop ended in an exception; the locals at
// the throw are read in the catch handler, which also checks the deopt state.
// Exception messages are printed only in the cold pass (HotSpot may omit them
// for hot implicit exceptions); later passes print the exception class.
//
// Passes: `cold` (every edge once), `warm` (60k calls of each kernel on safe
// arguments, so both tiers compile with the checks eliminated), `edge` (every
// edge again: the speculation must trap or must never have been made),
// `late` (Grow / Nuller hooks loaded), `rewarm`, `final`.
//
// Run:
//   javac -d out R21Fz4BceWrap1.java
//   java -Xmx256m -cp out R21Fz4BceWrap1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz4BceWrap1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz4BceWrap1
//   cratonvm --nojit -Xmx256m -cp out R21Fz4BceWrap1
//   CRATONVM_JIT_INCLUSIVE_BCE=1 CRATONVM_JIT_DOWN_COUNT_BCE=1 cratonvm -Xmx256m -cp out R21Fz4BceWrap1
//   CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION=1 cratonvm ...
//   CRATONVM_JIT_IR_BCE_STRIDE3=0 CRATONVM_JIT_IR_BCE_ROOM_OFFSET=0 cratonvm ...   # control
// HotSpot expectation: every line identical on every arm.

public class R21Fz4BceWrap1 {
    static final int WARM = 60_000;
    static final int MAX = Integer.MAX_VALUE;
    static final int MIN = Integer.MIN_VALUE;

    static String lastMsg;

    static int[] filled(int n) {
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i + 1;
        }
        return a;
    }

    static long code(int t, int s) {
        return t * 1_000_003L + s;
    }

    static long inclWrap(int[] a, int from, int to) {
        int s = 0, t = 0;
        try {
            for (int i = from; i <= to; i++) {
                s += a[i - from];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    static long strideWrap(int[] a, int from, int to, int st) {
        int s = 0, t = 0;
        try {
            for (int i = from; i < to; i += st) {
                s += a[i - from];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    static long downWrap(int[] a, int from, int to) {
        int s = 0, t = 0;
        try {
            for (int i = from; i >= to; i--) {
                s += a[from - i];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    static long offWrap(int[] a, int n, int off) {
        int s = 0, t = 0;
        try {
            for (int i = 0; i < n; i++) {
                s += a[i + off];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    static long offWrapStore(int[] a, int n, int off) {
        int t = 0;
        try {
            for (int i = 0; i < n; i++) {
                a[i + off] += i;
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, a[0]);
        }
        return code(t, a[0]);
    }

    static long mulWrap(int[] a, int from, int n, int base) {
        int s = 0, t = 0;
        try {
            for (int i = from; i < from + n; i++) {
                s += a[i * 3 - base];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    static long maskAll(int[] a, int n, int mask) {
        int s = 0, t = 0;
        try {
            for (int i = 0; i < n; i++) {
                s += a[i & mask];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    static long clampOff1(int[] a, int n, int lim) {
        int s = 0, t = 0;
        try {
            for (int i = 0; i < n; i++) {
                s += a[Math.min(i, lim)];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    static long clampNeg(int[] a, int n, int lo) {
        int s = 0, t = 0;
        try {
            for (int i = n - 1; i >= -n; i--) {
                s += a[Math.max(i, lo)];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return ~code(t, s);
        }
        return code(t, s);
    }

    // ---- field array replaced mid-loop ------------------------------------

    static class Hook {
        int[] replace(int[] cur, int i) {
            return cur;
        }
    }

    static final class Shrink extends Hook {
        int[] replace(int[] cur, int i) {
            return i == 5 ? new int[] {100, 200, 300} : cur;
        }
    }

    // Never touched before the `late` pass.
    static final class Grow extends Hook {
        int[] replace(int[] cur, int i) {
            if (i == 5) {
                int[] b = filled(cur.length * 2);
                b[6] = 1000;
                return b;
            }
            return cur;
        }
    }

    // Never touched before the `late` pass.
    static final class Nuller extends Hook {
        int[] replace(int[] cur, int i) {
            return i == 7 ? null : cur;
        }
    }

    static final class Holder {
        int[] arr;
        Hook hook;

        long walk() {
            int s = 0, t = 0;
            try {
                for (int i = 0; i < arr.length; i++) {
                    s += arr[i];
                    t++;
                    arr = hook.replace(arr, i);
                }
            } catch (RuntimeException e) {
                lastMsg = e.getClass().getName();
                return ~code(t, s);
            }
            return code(t, s);
        }

        long walkCached() {
            int s = 0, t = 0;
            int[] a = arr;
            try {
                for (int i = 0; i < a.length; i++) {
                    s += arr[i];
                    t++;
                    arr = hook.replace(arr, i);
                }
            } catch (RuntimeException e) {
                lastMsg = e.getClass().getName();
                return ~code(t, s);
            }
            return code(t, s);
        }
    }

    static long holder(Hook h, int n, boolean cached) {
        Holder o = new Holder();
        o.arr = filled(n);
        o.hook = h;
        return cached ? o.walkCached() : o.walk();
    }

    // ---- driver ---------------------------------------------------------------

    static String show(long r) {
        if (r < 0) {
            long c = ~r;
            return "EXC trips=" + (c / 1_000_003L) + " sum=" + (c % 1_000_003L);
        }
        return "ok trips=" + (r / 1_000_003L) + " sum=" + (r % 1_000_003L);
    }

    static void row(String pass, String name, long r, boolean msg) {
        String m = "";
        if (r < 0) {
            m = msg ? " msg=" + lastMsg : "";
        }
        System.out.println(pass + " " + name + " " + show(r) + m);
    }

    static void edges(String pass, boolean msg) {
        int[] a10 = filled(10);
        int[] a16 = filled(16);
        int[] a64 = filled(64);
        row(pass, "inclWrap.max", inclWrap(a10, MAX - 5, MAX), msg);
        row(pass, "inclWrap.maxShort", inclWrap(filled(3), MAX - 5, MAX), msg);
        row(pass, "inclWrap.maxM1", inclWrap(a10, MAX - 5, MAX - 1), msg);
        row(pass, "strideWrap.3", strideWrap(a16, MAX - 7, MAX, 3), msg);
        row(pass, "strideWrap.2", strideWrap(a16, MAX - 7, MAX, 2), msg);
        row(pass, "strideWrap.neg", strideWrap(a16, MIN + 2, MIN + 10, 5), msg);
        row(pass, "downWrap.min", downWrap(a10, MIN + 3, MIN), msg);
        row(pass, "downWrap.minP1", downWrap(a10, MIN + 3, MIN + 1), msg);
        row(pass, "offWrap.max", offWrap(a64, 8, MAX - 2), msg);
        row(pass, "offWrap.min", offWrap(a64, 8, MIN), msg);
        row(pass, "offWrap.neg1", offWrap(a64, 8, -1), msg);
        row(pass, "offWrap.end", offWrap(a64, 8, 60), msg);
        row(pass, "offWrapStore.max", offWrapStore(filled(64), 8, MAX - 2), msg);
        row(pass, "offWrapStore.end", offWrapStore(filled(64), 8, 58), msg);
        row(pass, "mulWrap.cross", mulWrap(a16, MAX / 3, 6, MAX - 1), msg);
        row(pass, "mulWrap.neg", mulWrap(a16, -3, 6, -9), msg);
        row(pass, "maskAll.m1", maskAll(a10, 12, -1), msg);
        row(pass, "maskAll.max", maskAll(a10, 12, MAX), msg);
        row(pass, "maskAll.min", maskAll(a10, 12, MIN), msg);
        row(pass, "clampOff1.len", clampOff1(a10, 12, 10), msg);
        row(pass, "clampOff1.neg", clampOff1(a10, 12, -1), msg);
        row(pass, "clampNeg.m1", clampNeg(a10, 10, -1), msg);
        row(pass, "clampNeg.min", clampNeg(a10, 10, MIN), msg);
        row(pass, "holder.shrink", holder(new Shrink(), 12, false), msg);
        row(pass, "holder.shrinkCached", holder(new Shrink(), 12, true), msg);
    }

    static long warm() {
        int[] a64 = filled(64);
        long h = 0;
        Hook keep = new Hook();
        Hook shrink = new Shrink();
        for (int k = 0; k < WARM; k++) {
            int f = 100 + (k & 7);
            h += inclWrap(a64, f, f + 40);
            h += strideWrap(a64, f, f + 60, 3);
            h += downWrap(a64, f + 50, f);
            h += offWrap(a64, 30, k & 31);
            h += offWrapStore(a64, 30, k & 31) & 0xff;
            h += mulWrap(a64, 10, 6, 30);
            h += maskAll(a64, 40, 63);
            h += clampOff1(a64, 50, 63);
            h += clampNeg(a64, 30, 0);
            if ((k & 15) == 0) {
                h += holder((k & 16) == 0 ? keep : shrink, 32, (k & 32) != 0);
            }
        }
        return h;
    }

    static void late(String pass) {
        row(pass, "holder.grow", holder(new Grow(), 12, false), false);
        row(pass, "holder.growCached", holder(new Grow(), 12, true), false);
        row(pass, "holder.null", holder(new Nuller(), 12, false), false);
        System.out.println(pass + " holder.null.cls " + lastMsg);
        row(pass, "holder.nullCached", holder(new Nuller(), 12, true), false);
        System.out.println(pass + " holder.nullCached.cls " + lastMsg);
        row(pass, "holder.keep", holder(new Hook(), 12, false), false);
        row(pass, "holder.shrink", holder(new Shrink(), 12, true), false);
    }

    public static void main(String[] args) {
        edges("cold", true);
        System.out.println("warm h=" + Long.toHexString(warm()));
        edges("edge", false);
        late("late");
        System.out.println("rewarm h=" + Long.toHexString(warm()));
        edges("final", false);
        late("final");
        System.out.println("done");
    }
}
