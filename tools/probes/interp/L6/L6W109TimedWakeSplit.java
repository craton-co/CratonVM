// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 109, lane L6 -- page
// docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md,
// "Wave 109". Splits the timed-wake gap of L6W108TimedParkWakeSplit (w108,
// Windows: --jdk-only 95 us, --compatible 47 us, HotSpot 39-47 us for
// `timed-unpark-wake`) by changing ONE thing per row against the base row:
//
//   base       peer `new Thread(lambda)` parks in parkNanos(10 s); main
//              Thread.sleep(5) then LockSupport.unpark(peer) (= W108's row)
//   untimed    the peer parks in LockSupport.park() instead
//   spin       main busy-waits 5 ms instead of sleeping (no idle processor
//              while the peer is parked: an idle-state cost disappears here)
//   ticker     as base, with a third thread looping Thread.sleep(1) (the
//              processors are woken every ms, as HotSpot's sleep(5) does
//              through timeBeginPeriod(1))
//   subclass   the peer is a Thread subclass overriding run() (no
//              Thread.run / runWith frames under it under --jdk-only; the
//              --compatible start stand-in's shape)
//   deep       the peer parks under 120 extra frames (a wake that rebuilds
//              the root snapshot grows with depth; the quiet leave does not)
//
// TIME lines: `TIME <row> median=<us>` (row names carry no digits, one line
// per row, so an A/B script keys on the text before the first number) and,
// on stderr, `TIME <row> p10=<us> p90=<us>`. The checksum line is
// deterministic.
//
// What decides what (interleave >= 5 runs per arm, medians; Windows; both
// modes; HotSpot `java L6W109TimedWakeSplit` is the reference):
//   * base ~ spin ~ ticker << sleep-only rows: nothing idle-related; look at
//     subclass / deep / untimed.
//   * spin and ticker near HotSpot, base not: the peer's processor sleeps
//     deeper here than under HotSpot, whose sleep(5) raises the clock rate;
//     arm `CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL=1` should then bring base
//     down to ticker.
//   * subclass near --compatible's base under --jdk-only: the JDK's
//     Thread.run frames under the park are the mode difference.
//   * deep >> base: the wake rebuilds the snapshot (check `[PB-QUIET]` with
//     CRATONVM_DBG_PB=1).
//
// HOTSPOT_EXPECTED_BEGIN
// checksum base=60 untimed=60 spin=60 ticker=60 subclass=60 deep=60
// done
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W109TimedWakeSplit {
    static final int WAKES = 60;
    static volatile long wokeAt;
    static volatile int phase;
    static volatile boolean ticking;

    enum Kind { BASE, UNTIMED, SPIN, TICKER, SUBCLASS, DEEP }

    static final class Peer implements Runnable {
        final Kind kind;
        final AtomicInteger woken;

        Peer(Kind kind, AtomicInteger woken) {
            this.kind = kind;
            this.woken = woken;
        }

        @Override
        public void run() {
            if (kind == Kind.DEEP) {
                down(120);
            } else {
                rounds();
            }
        }

        void down(int n) {
            if (n == 0) {
                rounds();
            } else {
                down(n - 1);
            }
        }

        void rounds() {
            for (int i = 0; i < WAKES; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                while (true) {
                    if (kind == Kind.UNTIMED) {
                        LockSupport.park();
                    } else {
                        LockSupport.parkNanos(10_000_000_000L);
                    }
                    if (wokeAt == -1) {
                        break;
                    }
                }
                wokeAt = System.nanoTime();
                woken.incrementAndGet();
                phase = 2 * i + 2;
            }
        }
    }

    static final class SubclassPeer extends Thread {
        final Peer body;

        SubclassPeer(Peer body, String name) {
            super(name);
            this.body = body;
        }

        @Override
        public void run() {
            body.rounds();
        }
    }

    static int wakeRow(String row, Kind kind) throws Exception {
        long[] lat = new long[WAKES];
        AtomicInteger woken = new AtomicInteger();
        phase = 0;
        Peer body = new Peer(kind, woken);
        Thread peer = kind == Kind.SUBCLASS
                ? new SubclassPeer(body, "w109-" + row)
                : new Thread(body, "w109-" + row);
        Thread ticker = null;
        if (kind == Kind.TICKER) {
            ticking = true;
            ticker = new Thread(() -> {
                while (ticking) {
                    try {
                        Thread.sleep(1);
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }, "w109-ticker");
            ticker.setDaemon(true);
            ticker.start();
        }
        peer.start();
        for (int i = 0; i < WAKES; i++) {
            wokeAt = 0;
            phase = 2 * i + 1;
            // Past the 2 ms running phase of the park: the blocking wait.
            if (kind == Kind.SPIN) {
                long until = System.nanoTime() + 5_000_000L;
                while (System.nanoTime() < until) {
                    Thread.onSpinWait();
                }
            } else {
                Thread.sleep(5);
            }
            long t0 = System.nanoTime();
            wokeAt = -1;
            LockSupport.unpark(peer);
            while (phase != 2 * i + 2) {
                Thread.onSpinWait();
            }
            lat[i] = wokeAt - t0;
        }
        peer.join();
        if (ticker != null) {
            ticking = false;
            ticker.join();
        }
        Arrays.sort(lat);
        System.out.println("TIME " + row + " median=" + lat[WAKES / 2] / 1_000);
        System.err.println("TIME " + row + " p10=" + lat[WAKES / 10] / 1_000
                + " p90=" + lat[WAKES * 9 / 10] / 1_000);
        return woken.get();
    }

    public static void main(String[] args) throws Exception {
        StringBuilder sum = new StringBuilder("checksum");
        for (Kind k : Kind.values()) {
            String row = k.name().toLowerCase(java.util.Locale.ROOT);
            sum.append(' ').append(row).append('=').append(wakeRow(row, k));
        }
        System.out.println(sum);
        System.out.println("done");
    }
}
