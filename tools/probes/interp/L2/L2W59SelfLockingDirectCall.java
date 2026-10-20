// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L2 -- the correctness probe of
// `CRATONVM_JIT_SELF_LOCKING_DIRECT_BIND_RETRY` (the i58-L2 proposal "a hot
// synchronized callee that calls gets a direct call"): an optimizing caller
// binds `StringBuffer.append(C)` / `length()` / `setLength(I)` -- synchronized
// JDK methods whose published bodies are SELF-LOCKING -- as plain direct
// CALLs on its FIRST compile, instead of blind dispatch. The calls must stay
// correctly locked (two threads append to one buffer), keep the exception
// path's release (a failing `setLength(-1)` inside a hot loop, then another
// thread must still get the monitor), and leave no monitor held.
//
// Rows (each first cold, then hot; counts in `long`):
//   cold  one thread, 1000 appends;
//   race  two threads x 200 000 appends to one buffer, length sum;
//   throw a hot loop whose every 1000th iteration calls setLength(-1), caught;
//         then another thread appends under the same monitor;
//   held  Thread.holdsLock(buffer) after the loops.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3, default flags; the same with -Xint)
// cold length=1000
// race total=400000
// throw caught=200 other-thread-appended=true
// held false
// cold length=1000
// HOTSPOT_EXPECTED_END
//
// POSITIVE CONTROL: `CRATONVM_DBG_JITC=1` prints
// `[cratonvm-jitc] ir-direct-call SELF-LOCKING-RETRY java/lang/StringBuffer.append(C)Ljava/lang/StringBuffer; @pc=..`
// for the optimizing compile of `spin` with the switch on; off, that compile
// prints `sync-direct REFUSED ... published body not closed (... wrapped=false)`
// and `ir-direct-call MISSED` for the same site.
public class L2W59SelfLockingDirectCall {
    static final long N = 200_000L;
    static volatile long sink;

    static long spin(StringBuffer b, long n) {
        long total = 0;
        for (long i = 0; i < n; i++) {
            b.append('x');
            if (b.length() >= 4096) {
                total += b.length();
                b.setLength(0);
            }
        }
        return total + b.length();
    }

    static long throwing(StringBuffer b, long n) {
        long caught = 0;
        for (long i = 0; i < n; i++) {
            b.append('y');
            if (i % 1000 == 999) {
                try {
                    b.setLength(-1);
                } catch (IndexOutOfBoundsException e) {
                    caught++;
                }
                b.setLength(0);
            }
        }
        return caught;
    }

    static void cold() {
        StringBuffer b = new StringBuffer();
        System.out.println("cold length=" + spin(b, 1000));
    }

    public static void main(String[] args) throws Exception {
        cold();
        // Warm the callee bodies and `spin` alone first.
        StringBuffer warm = new StringBuffer();
        for (int r = 0; r < 20; r++) {
            sink += spin(warm, N / 10);
        }
        // Two threads append to one buffer through the same compiled sites
        // (`append`, `length`); no reset, so the final length is exact.
        Thread[] ts = new Thread[2];
        StringBuffer counted = new StringBuffer();
        long[] appended = new long[2];
        for (int t = 0; t < 2; t++) {
            final int k = t;
            ts[t] = new Thread(() -> {
                long c = 0;
                for (long i = 0; i < N; i++) {
                    counted.append('z');
                    c++;
                    if ((i & 1023) == 1023) {
                        sink += counted.length();
                    }
                }
                appended[k] = c;
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("race total=" + counted.length());

        StringBuffer thrower = new StringBuffer();
        long caught = throwing(thrower, N);
        Thread other = new Thread(() -> thrower.append("ok"));
        other.start();
        other.join(10_000);
        System.out.println("throw caught=" + caught + " other-thread-appended="
                + thrower.toString().endsWith("ok"));
        System.out.println("held " + (Thread.holdsLock(counted) || Thread.holdsLock(thrower)
                || Thread.holdsLock(warm)));
        cold();
        sink += appended[0] + appended[1];
    }
}
