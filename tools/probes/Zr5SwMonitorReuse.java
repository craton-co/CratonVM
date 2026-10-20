// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z5/sw (2026-10-12): the ZGC monitor prune after the key probe.
//
// `CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL` (default ON, `=0` kill) stops the
// ZGC sweep from listing every dead base for `MonitorTable::prune_dead`; the
// collector asks for the monitor/cas-lock KEYS instead and hands the prune
// `keys ∩ dead`. If that intersection were ever short, a dead object's
// inflated monitor would stay in the index at its address, and a NEW object
// allocated there would inherit it (a non-idle inherited monitor deadlocks the
// new object's first `synchronized`, an owned one throws
// IllegalMonitorStateException). If it were ever long, a LIVE object's monitor
// would be dropped while in use.
//
// Each round: inflate monitors on objects that then die (wait(1) inflates),
// keep one inflated survivor per round, churn the heap so whole-heap cycles
// recycle the dead addresses, then lock fresh objects (many land on recycled
// addresses), ping-pong wait/notify across two threads on fresh objects, and
// re-check every kept survivor's monitor (owner, wait/notify handoff). Every
// line printed is deterministic and must equal HotSpot's.
//
// Usage: java -XX:+UseZGC -Xmx256m Zr5SwMonitorReuse [rounds=40] [perRound=4000] [churnMb=96]
// Rows (orchestrator): ON vs CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL=0, both modes;
// `CRATONVM_GC_STATS=1` prints `[GC] zgc-sweep-live: prune_key_cycles=` (> 0 ON,
// 0 at `=0`: the positive / kill-switch control).
import java.util.ArrayList;
import java.util.List;

public final class Zr5SwMonitorReuse {
    static volatile Object sink;

    static int arg(String[] a, int i, int d) {
        return a.length > i ? Integer.parseInt(a[i]) : d;
    }

    /// Two-thread handoff on `o`: the helper waits for `flag[0]`, main sets it
    /// and notifies. Returns true when the helper finished in time.
    static boolean pingPong(Object o) throws InterruptedException {
        final boolean[] flag = new boolean[1];
        final boolean[] seen = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (o) {
                while (!flag[0]) {
                    try {
                        o.wait(50);
                    } catch (InterruptedException e) {
                        return;
                    }
                }
                seen[0] = Thread.holdsLock(o);
            }
        });
        t.setDaemon(true);
        t.start();
        Thread.sleep(1);
        synchronized (o) {
            flag[0] = true;
            o.notifyAll();
        }
        t.join(10_000);
        return !t.isAlive() && seen[0];
    }

    static void churn(int mb) {
        long bytes = (long) mb << 20;
        long done = 0;
        int i = 0;
        while (done < bytes) {
            int len = (i++ * 7) % 65;
            sink = new int[len];
            done += 16 + 4L * len;
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = arg(args, 0, 40);
        int perRound = arg(args, 1, 4000);
        int churnMb = arg(args, 2, 96);
        List<Object> kept = new ArrayList<>();
        long badLock = 0, badPing = 0, badKept = 0, imse = 0, fresh = 0, pings = 0;
        for (int r = 0; r < rounds; r++) {
            Object[] objs = new Object[perRound];
            for (int i = 0; i < perRound; i++) {
                objs[i] = new Object();
            }
            // Inflate 16 of them (wait(1) inflates on every collector here);
            // all but the first die with the array.
            for (int k = 0; k < 16; k++) {
                Object o = objs[k * (perRound / 16)];
                synchronized (o) {
                    o.wait(1);
                }
            }
            kept.add(objs[0]);
            objs = null;
            churn(churnMb);
            // Fresh objects, many at recycled addresses: an inherited monitor
            // shows up here as a hang, an IMSE, or holdsLock lying.
            for (int i = 0; i < perRound; i++) {
                Object o = new Object();
                try {
                    synchronized (o) {
                        if (!Thread.holdsLock(o)) {
                            badLock++;
                        }
                        if ((i & 255) == 0) {
                            o.notify(); // owned: must not throw
                        }
                    }
                } catch (IllegalMonitorStateException e) {
                    imse++;
                }
                fresh++;
            }
            for (int k = 0; k < 2; k++) {
                pings++;
                if (!pingPong(new Object())) {
                    badPing++;
                }
            }
            // The kept survivors' monitors must still work.
            for (Object o : kept) {
                synchronized (o) {
                    if (!Thread.holdsLock(o)) {
                        badKept++;
                    }
                    o.notifyAll();
                }
            }
            if ((r & 7) == 7 && !pingPong(kept.get(r / 2))) {
                badKept++;
            }
        }
        System.out.println("zr5sw-monitor-reuse: rounds=" + rounds + " fresh=" + fresh
                + " pings=" + pings + " kept=" + kept.size() + " bad_lock=" + badLock
                + " bad_ping=" + badPing + " bad_kept=" + badKept + " imse=" + imse);
    }
}
