// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L6 -- diagnostic bench for the
// --compatible unpark-wake latency seen in wave 74 (L6W74ParkNoPollBench:
// --compatible unpark-wake min ~84 us / median ~950 us with the poll, and
// worse with CRATONVM_PARK_UNTIMED_NO_POLL=1, while interrupt-wake with the
// switch was ~69 us). Page:
// docs/internal/fixed-bugs/interpreter-i73-L6-proposal-untimed-park-without-a-5ms-poll-DONE-20261115.md
// ("Progress (wave 75)").
//
// The rows split the wake so one run says WHICH part is slow:
//   unpark-call       median ns main spends inside LockSupport.unpark(peer);
//   unpark-wake       median ns from the unpark call to the parked peer
//                     running (as W74's row);
//   unpark-wake-twice the same with TWO unpark calls back to back. Each
//                     unpark notifies the condvar once; if this row is fast
//                     where unpark-wake is slow, the first notify is landing
//                     on another waiter of the peer's park state (a stolen
//                     notify_one) -- the case wave 75's notify_all under
//                     CRATONVM_PARK_UNTIMED_NO_POLL fixes;
//   interrupt-wake    the same for Thread.interrupt();
// and on stderr, per row, "late-parks=N/200": rounds whose LAST park() call
// began after the wake call (the peer was not yet in its wait: the latency is
// the peer's way into the park, not the wake), and "park-calls=N" (park()
// returns per round beyond the first: spurious returns).
//
// A/B, one binary, interleaved, medians; --jdk-only and --compatible:
//   cratonvm L6W75UnparkWakeSplit
//   CRATONVM_PARK_UNTIMED_NO_POLL=1 cratonvm L6W75UnparkWakeSplit
//   CRATONVM_DBG_PARKLAT=300 cratonvm --compatible L6W75UnparkWakeSplit 2>&1 | grep parklat
// The last prints every park whose wake came >= 300 us after its unpark
// ("woke_by=timeout-tick": the notify did not end the wait, a poll slice
// found the permit; "woke_by=notify": the wake itself was late) and every
// blocking prologue / post-wake epilogue >= 300 us.
//
// Interpreter round i5 wave 105, lane L6: every timing line (stdout rows and
// the stderr diagnostics) now starts with "TIME ", so the battery filters
// them and only the checksum lines compare.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum unpark-wakes=200 twice-wakes=200 interrupt-wakes=200
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W75UnparkWakeSplit {
    static final int WAKES = 200;

    static volatile long wokeAt;
    static volatile long lastParkAt;
    static volatile int phase;

    enum Kind { UNPARK, TWICE, INTERRUPT }

    static int wakeRow(String row, Kind kind) throws InterruptedException {
        long[] lat = new long[WAKES];
        long[] call = new long[WAKES];
        int late = 0;
        AtomicInteger woken = new AtomicInteger();
        AtomicInteger extraParks = new AtomicInteger();
        phase = 0;
        Thread peer = new Thread(() -> {
            for (int i = 0; i < WAKES; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                int parks = 0;
                while (phase == 2 * i + 1) {
                    lastParkAt = System.nanoTime();
                    LockSupport.park();
                    parks++;
                    if (kind == Kind.INTERRUPT) {
                        if (Thread.interrupted()) {
                            break;
                        }
                    } else if (wokeAt == -1) {
                        break;
                    }
                }
                wokeAt = System.nanoTime();
                extraParks.addAndGet(parks - 1);
                woken.incrementAndGet();
                phase = 2 * i + 2;
            }
        }, "w75-" + row);
        peer.start();
        for (int i = 0; i < WAKES; i++) {
            wokeAt = 0;
            phase = 2 * i + 1;
            // Well past the 2 ms running phase: the blocking wait.
            Thread.sleep(5);
            long t0 = System.nanoTime();
            if (kind == Kind.INTERRUPT) {
                peer.interrupt();
            } else {
                wokeAt = -1;
                LockSupport.unpark(peer);
                if (kind == Kind.TWICE) {
                    LockSupport.unpark(peer);
                }
            }
            long t1 = System.nanoTime();
            while (phase != 2 * i + 2) {
                Thread.onSpinWait();
            }
            lat[i] = wokeAt - t0;
            call[i] = t1 - t0;
            if (lastParkAt > t0) {
                late++;
            }
        }
        peer.join();
        Arrays.sort(lat);
        Arrays.sort(call);
        if (kind == Kind.UNPARK) {
            System.out.printf("TIME %-18s %.1f ns/op%n", "unpark-call", (double) call[WAKES / 2]);
        }
        System.out.printf("TIME %-18s %.1f ns/op%n", row, (double) lat[WAKES / 2]);
        System.err.println("TIME " + row + " late-parks=" + late + "/" + WAKES
                + " park-calls=" + extraParks.get()
                + " p10=" + lat[WAKES / 10] + " p90=" + lat[WAKES * 9 / 10]);
        return woken.get();
    }

    public static void main(String[] args) throws Exception {
        int unpark = wakeRow("unpark-wake", Kind.UNPARK);
        int twice = wakeRow("unpark-wake-twice", Kind.TWICE);
        int interrupt = wakeRow("interrupt-wake", Kind.INTERRUPT);
        System.out.println("checksum unpark-wakes=" + unpark + " twice-wakes=" + twice
                + " interrupt-wakes=" + interrupt);
        System.out.println("done");
    }
}
