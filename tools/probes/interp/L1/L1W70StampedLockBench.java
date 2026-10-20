// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 70, lane L1 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i69-L1-proposal-compatible-stampedlock-runs-its-jdk-bodies-DONE-20261102.md
// (the proposal's "Risk": the `--compatible` stand-ins kept the lock in a
// side table under a Rust mutex; the `JdkStampedLock` row replaces them with
// the JDK 25 bodies, CAS on `state`).
//
// A/B, one binary, `--compatible` over a real JDK:
//   cratonvm --compatible L1W70StampedLockBench                                    (row on, default)
//   CRATONVM_COMPAT_JDK_STAMPED_LOCK=0 cratonvm --compatible L1W70StampedLockBench (stand-ins)
// and `cratonvm L1W70StampedLockBench` (jdk-only) as the reference that must
// equal the row-on arm. Interleave the arms and take medians of the per-run
// minimums (in-JVM timings swing ~3x on a noisy host).
//
// Rows (stderr, min ns/op over REPS=7 rounds):
//   optimistic   tryOptimisticRead + validate, uncontended;
//   read         readLock + unlockRead, uncontended;
//   write        writeLock + unlockWrite, uncontended;
//   mixed4       4 threads, 9 reads : 1 write each, ns per operation (wall);
//
// stdout carries only checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum optimistic=200000 read=200000 write=200000 mixed4-writes=8000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.StampedLock;

public class L1W70StampedLockBench {
    static final int REPS = 7;
    static final int N = 200_000;
    static final int THREADS = 4;
    static final int MIXED_PER_THREAD = 20_000;

    static long value;

    static long optimistic(StampedLock sl) {
        long ok = 0;
        for (int i = 0; i < N; i++) {
            long s = sl.tryOptimisticRead();
            long v = value;
            if (sl.validate(s)) {
                ok += 1 + (v & 0);
            }
        }
        return ok;
    }

    static long read(StampedLock sl) {
        long n = 0;
        for (int i = 0; i < N; i++) {
            long s = sl.readLock();
            n += 1 + (value & 0);
            sl.unlockRead(s);
        }
        return n;
    }

    static long write(StampedLock sl) {
        long n = 0;
        for (int i = 0; i < N; i++) {
            long s = sl.writeLock();
            value++;
            n++;
            sl.unlockWrite(s);
        }
        return n;
    }

    static long mixed(StampedLock sl) throws InterruptedException {
        long[] writes = new long[THREADS];
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long w = 0;
                for (int i = 0; i < MIXED_PER_THREAD; i++) {
                    if (i % 10 == 9) {
                        long s = sl.writeLock();
                        value++;
                        sl.unlockWrite(s);
                        w++;
                    } else {
                        long s = sl.readLock();
                        long v = value;
                        sl.unlockRead(s);
                        if (v < 0) {
                            w = -1;
                        }
                    }
                }
                writes[id] = w;
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long sum = 0;
        for (long w : writes) {
            sum += w;
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        StampedLock sl = new StampedLock();
        long best[] = {Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE};
        long co = 0, cr = 0, cw = 0, cm = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            co = optimistic(sl);
            long t1 = System.nanoTime();
            cr = read(sl);
            long t2 = System.nanoTime();
            cw = write(sl);
            long t3 = System.nanoTime();
            cm = mixed(sl);
            long t4 = System.nanoTime();
            best[0] = Math.min(best[0], t1 - t0);
            best[1] = Math.min(best[1], t2 - t1);
            best[2] = Math.min(best[2], t3 - t2);
            best[3] = Math.min(best[3], t4 - t3);
        }
        System.err.printf("optimistic %.1f ns/op%n", best[0] / (double) N);
        System.err.printf("read       %.1f ns/op%n", best[1] / (double) N);
        System.err.printf("write      %.1f ns/op%n", best[2] / (double) N);
        System.err.printf("mixed4     %.1f ns/op%n", best[3] / (double) (THREADS * MIXED_PER_THREAD));
        System.out.println("checksum optimistic=" + co + " read=" + cr + " write=" + cw + " mixed4-writes=" + cm);
        System.out.println("done");
    }
}
