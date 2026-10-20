// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: INDY21-3, the static-init waiter that missed
// a class initialized DURING the compile (`CRATONVM_JIT_STATIC_INIT_RACE_SERVE`,
// default ON, `=0` restores the race; `jit_bridge::StaticInitLog`,
// `note_static_init_waiters`), with the IR clinit barrier and the
// STATIC_INIT_RECOMPILE path it feeds.
//
// Risk targeted: a reader method compiled while its holders are
// uninitialized takes the helper route, then is recompiled once the holders'
// `<clinit>` end (inline reads, the trusted array length, constant folds).
// The recompiled body must read the INITIALIZED values: a recompile that
// folded a `static final` array length or element from before `<clinit>`
// finished, or a log entry written before the initialized bit, would show a
// stale 0 / null. Eight holders with `static final int[]` tables and
// `static final` scalars are initialized by four threads concurrently while
// the main thread spins the hot reader over all of them (reads that block
// until each holder is ready). Sums are exact and printed per holder.
//
// Run:
//   javac -d out R21FzIndyClinit2.java
//   java -Xmx256m -cp out R21FzIndyClinit2                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzIndyClinit2
//   CRATONVM_JIT_STATIC_INIT_RACE_SERVE=0 cratonvm -Xmx256m -cp out R21FzIndyClinit2
//   CRATONVM_JIT_IR_CLINIT_BARRIER=1 CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzIndyClinit2 {
    static int[] build(int k, int n) {
        int[] t = new int[n];
        for (int i = 0; i < n; i++) {
            t[i] = (i * 31 + k) ^ (k << 8);
        }
        return t;
    }

    static int slow(int k) {
        long s = 0;
        for (int i = 0; i < 200_000 * (k + 1); i++) {
            s += i ^ k;
        }
        return (int) (s & 0xFFFF);
    }

    static final class H0 { static final int[] T = build(0, 17); static final int K = slow(0); static final String S = "h0"; }
    static final class H1 { static final int[] T = build(1, 33); static final int K = slow(1); static final String S = "h1"; }
    static final class H2 { static final int[] T = build(2, 5); static final int K = slow(2); static final String S = "h2"; }
    static final class H3 { static final int[] T = build(3, 64); static final int K = slow(3); static final String S = "h3"; }
    static final class H4 { static final int[] T = build(4, 1); static final int K = slow(4); static final String S = "h4"; }
    static final class H5 { static final int[] T = build(5, 100); static final int K = slow(5); static final String S = "h5"; }
    static final class H6 { static final int[] T = build(6, 9); static final int K = slow(6); static final String S = "h6"; }
    static final class H7 { static final int[] T = build(7, 48); static final int K = slow(7); static final String S = "h7"; }

    static long read(int h, int i) {
        switch (h) {
            case 0: return H0.T[i % H0.T.length] + H0.K + H0.S.length();
            case 1: return H1.T[i % H1.T.length] + H1.K + H1.S.length();
            case 2: return H2.T[i % H2.T.length] + H2.K + H2.S.length();
            case 3: return H3.T[i % H3.T.length] + H3.K + H3.S.length();
            case 4: return H4.T[i % H4.T.length] + H4.K + H4.S.length();
            case 5: return H5.T[i % H5.T.length] + H5.K + H5.S.length();
            case 6: return H6.T[i % H6.T.length] + H6.K + H6.S.length();
            default: return H7.T[i % H7.T.length] + H7.K + H7.S.length();
        }
    }

    static long expect(int h, int i) {
        int n = new int[] {17, 33, 5, 64, 1, 100, 9, 48}[h];
        return build(h, n)[i % n] + slowCached(h) + 2;
    }

    static final int[] SLOW = new int[8];

    static int slowCached(int h) {
        return SLOW[h];
    }

    /** Reads only holders 0..k-1 (already initialized) to get read() hot first. */
    public static void main(String[] args) throws Exception {
        for (int h = 0; h < 8; h++) {
            SLOW[h] = slow(h);
        }
        // Warm the reader on holder 0 and 1 only (initialized here).
        long w = 0;
        for (int i = 0; i < 300_000; i++) {
            w += read(i & 1, i);
        }
        System.out.println("warm " + w);
        Thread[] ts = new Thread[4];
        long[] got = new long[4];
        for (int t = 0; t < 4; t++) {
            final int tt = t;
            ts[t] = new Thread(() -> {
                long s = 0;
                for (int i = 0; i < 50_000; i++) {
                    s += read(2 + ((i + tt) % 6), i);
                }
                got[tt] = s;
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        long mainSum = 0;
        for (int i = 0; i < 400_000; i++) {
            mainSum += read(i & 7, i);
        }
        for (Thread t : ts) {
            t.join();
        }
        long want = 0;
        for (int i = 0; i < 400_000; i++) {
            want += expect(i & 7, i);
        }
        System.out.println("main " + mainSum + (mainSum == want ? " ok" : " MISMATCH want " + want));
        for (int t = 0; t < 4; t++) {
            long e = 0;
            for (int i = 0; i < 50_000; i++) {
                e += expect(2 + ((i + t) % 6), i);
            }
            System.out.println("thread" + t + " " + got[t] + (got[t] == e ? " ok" : " MISMATCH want " + e));
        }
        for (int h = 0; h < 8; h++) {
            long s = 0;
            for (int i = 0; i < 100_000; i++) {
                s += read(h, i);
            }
            System.out.println("holder" + h + " " + s);
        }
    }
}
