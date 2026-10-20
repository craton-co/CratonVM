// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L6 -- bench for
// CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK (page
// docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md,
// "Wave 108"). L6W75UnparkWakeSplit's shape for a TIMED park: a peer parked
// in LockSupport.parkNanos(10 s) -- on Windows one coarse wait on its park
// state's wake event -- is woken by
//
//   timed-unpark-wake     LockSupport.unpark(peer)
//   timed-interrupt-wake  peer.interrupt()
//   timed-future-wake     CompletableFuture.complete on a future the peer
//                         waits for with get(10 s) (the JDK's timed Signaller)
//
// TIME lines: the median and p10/p90 ns from the wake call to the peer
// running again. The checksum line is deterministic.
//
// A/B (Windows; one binary, interleaved, medians of >= 5; both modes):
//   cratonvm L6W108TimedParkWakeSplit
//   CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK=1 cratonvm L6W108TimedParkWakeSplit
//   (java L6W108TimedParkWakeSplit for HotSpot)
//
// HOTSPOT_EXPECTED_BEGIN
// checksum timed-unpark=100 timed-interrupt=100 timed-future=100
// done
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;

public class L6W108TimedParkWakeSplit {
    static final int WAKES = 100;
    static volatile long wokeAt;
    static volatile int phase;

    enum Kind { UNPARK, INTERRUPT, FUTURE }

    static int wakeRow(String row, Kind kind) throws Exception {
        long[] lat = new long[WAKES];
        AtomicInteger woken = new AtomicInteger();
        AtomicReference<CompletableFuture<String>> future = new AtomicReference<>();
        phase = 0;
        Thread peer = new Thread(() -> {
            for (int i = 0; i < WAKES; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                if (kind == Kind.FUTURE) {
                    try {
                        future.get().get(10, TimeUnit.SECONDS);
                    } catch (Exception e) {
                        // counted as not woken below
                        wokeAt = System.nanoTime();
                        phase = 2 * i + 2;
                        continue;
                    }
                } else {
                    while (true) {
                        LockSupport.parkNanos(10_000_000_000L);
                        if (kind == Kind.INTERRUPT) {
                            if (Thread.interrupted()) {
                                break;
                            }
                        } else if (wokeAt == -1) {
                            break;
                        }
                    }
                }
                wokeAt = System.nanoTime();
                woken.incrementAndGet();
                phase = 2 * i + 2;
            }
        }, "w108-" + row);
        peer.start();
        for (int i = 0; i < WAKES; i++) {
            wokeAt = 0;
            CompletableFuture<String> f = new CompletableFuture<>();
            future.set(f);
            phase = 2 * i + 1;
            // Past the 2 ms running phase: the blocking wait.
            Thread.sleep(5);
            long t0 = System.nanoTime();
            switch (kind) {
                case INTERRUPT -> peer.interrupt();
                case FUTURE -> f.complete("x");
                default -> {
                    wokeAt = -1;
                    LockSupport.unpark(peer);
                }
            }
            while (phase != 2 * i + 2) {
                Thread.onSpinWait();
            }
            lat[i] = wokeAt - t0;
        }
        peer.join();
        Arrays.sort(lat);
        System.out.printf("TIME %-22s median=%d ns p10=%d p90=%d%n", row, lat[WAKES / 2],
                lat[WAKES / 10], lat[WAKES * 9 / 10]);
        return woken.get();
    }

    public static void main(String[] args) throws Exception {
        int unpark = wakeRow("timed-unpark-wake", Kind.UNPARK);
        int interrupt = wakeRow("timed-interrupt-wake", Kind.INTERRUPT);
        int future = wakeRow("timed-future-wake", Kind.FUTURE);
        System.out.println("checksum timed-unpark=" + unpark + " timed-interrupt=" + interrupt
                + " timed-future=" + future);
        System.out.println("done");
    }
}
