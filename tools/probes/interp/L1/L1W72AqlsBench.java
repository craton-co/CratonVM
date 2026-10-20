// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L1 -- bench probe for the `JdkAqls` row
// (`CRATONVM_COMPAT_JDK_AQLS`, default on): over a real JDK `--compatible`
// runs the JDK 25 `AbstractQueuedLongSynchronizer.getState` / `setState` /
// `compareAndSetState` bodies (a field read, a field write, an `Unsafe` long
// CAS) instead of the stand-ins (a native door, a GC-stable identity key and a
// process-global `Mutex<HashMap>` lookup per call).
//
// A/B, one binary, `--compatible` over a real JDK:
//   cratonvm --compatible L1W72AqlsBench                              (row on, default)
//   CRATONVM_COMPAT_JDK_AQLS=0 cratonvm --compatible L1W72AqlsBench   (stand-ins)
// and `cratonvm L1W72AqlsBench` (jdk-only) as the reference the row-on arm
// should equal. Interleave the arms and take medians of the per-run minimums
// (in-JVM timings swing ~3x on a noisy host). Add `--nojit` for the
// interpreter-only picture.
//
// Rows (stdout, min ns/op over REPS=7 rounds):
//   get-state        getState() on a subclass;
//   set-state        setState(i);
//   cas-state        compareAndSetState(i, i + 1), always succeeding;
//   acquire-release  acquire(1); release(1) on an uncontended one-holder lock;
//   contended2       two threads x N/4 acquire/release pairs on one lock, ns per pair.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum get-state=200000 set-state=199999 cas-state=200000 acquire-release=200000 contended2=100000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.AbstractQueuedLongSynchronizer;

public class L1W72AqlsBench {
    static final int REPS = 7;
    static final int N = 200_000;

    static final class LongLock extends AbstractQueuedLongSynchronizer {
        private static final long serialVersionUID = 1L;

        @Override
        protected boolean tryAcquire(long arg) {
            if (compareAndSetState(0, arg)) {
                setExclusiveOwnerThread(Thread.currentThread());
                return true;
            }
            return false;
        }

        @Override
        protected boolean tryRelease(long arg) {
            setExclusiveOwnerThread(null);
            setState(0);
            return true;
        }

        @Override
        protected boolean isHeldExclusively() {
            return getExclusiveOwnerThread() == Thread.currentThread();
        }

        long state() {
            return getState();
        }

        void state(long v) {
            setState(v);
        }

        boolean cas(long expect, long update) {
            return compareAndSetState(expect, update);
        }
    }

    static long getState(LongLock l) {
        l.state(1);
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += l.state();
        }
        return sum;
    }

    static long setState(LongLock l) {
        for (int i = 0; i < N; i++) {
            l.state(i);
        }
        return l.state();
    }

    static long casState(LongLock l) {
        l.state(0);
        long ok = 0;
        for (int i = 0; i < N; i++) {
            if (l.cas(i, i + 1)) {
                ok++;
            }
        }
        return ok;
    }

    static long acquireRelease(LongLock l) {
        l.state(0);
        long n = 0;
        for (int i = 0; i < N; i++) {
            l.acquire(1);
            n++;
            l.release(1);
        }
        return n;
    }

    static long contended(LongLock l) throws InterruptedException {
        l.state(0);
        long[] count = {0};
        Runnable body = () -> {
            for (int i = 0; i < N / 4; i++) {
                l.acquire(1);
                count[0]++;
                l.release(1);
            }
        };
        Thread a = new Thread(body, "aqls-a");
        Thread b = new Thread(body, "aqls-b");
        a.start();
        b.start();
        a.join();
        b.join();
        return count[0];
    }

    public static void main(String[] args) throws Exception {
        LongLock l = new LongLock();
        String[] names = {"get-state", "set-state", "cas-state", "acquire-release", "contended2"};
        long[] per = {N, N, N, N, N / 2};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        for (int r = 0; r < REPS; r++) {
            long[] t = new long[names.length + 1];
            t[0] = System.nanoTime();
            sums[0] = getState(l);
            t[1] = System.nanoTime();
            sums[1] = setState(l);
            t[2] = System.nanoTime();
            sums[2] = casState(l);
            t[3] = System.nanoTime();
            sums[3] = acquireRelease(l);
            t[4] = System.nanoTime();
            sums[4] = contended(l);
            t[5] = System.nanoTime();
            for (int i = 0; i < names.length; i++) {
                best[i] = Math.min(best[i], t[i + 1] - t[i]);
            }
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-16s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
        }
        System.out.println("checksum get-state=" + sums[0] + " set-state=" + sums[1]
                + " cas-state=" + sums[2] + " acquire-release=" + sums[3]
                + " contended2=" + sums[4]);
        System.out.println("done");
    }
}
