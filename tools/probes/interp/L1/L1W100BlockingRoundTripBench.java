// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L1 -- bench for
// docs/internal/fixed-bugs/interpreter-i100-L1-proposal-a-blocked-thread-is-walked-not-deposited-CLOSED-20261224.md
// and the quiet leave (CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT).
//
// Two threads hand a token back and forth through three blocking doors
// (LockSupport.park/unpark, Object.wait/notify, a SynchronousQueue), with the
// blocking call at the bottom of a shallow (4) and a deep (200) recursion.
// Every hand-off is one blocking-region entry and one leave per thread, so
// ns per round trip, and how much it grows from depth 4 to 200, is the cost
// of the region's deposit (entry) and wake (leave). HotSpot's does not grow
// with depth.
//
// Deterministic stdout (compare with HotSpot); timings on "TIME " lines only.
//   javac -d out L1W100BlockingRoundTripBench.java
//   cratonvm -cp out L1W100BlockingRoundTripBench
//   (A/B: CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=0; interleave the binaries
//   and take medians over several runs, in-VM timings swing ~3x here)
//
// A/B in one command (wave 100 follow-up): `ab` mode re-runs the plain mode
// as children of this VM's launcher, 5 interleaved reps of three arms --
// keep+reuse (quiet leave + CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS=1),
// keep (quiet leave only), rebuild (quiet leave off) -- and prints per-row
// medians on "TIME ab <kind> depth=<d> arm=<arm> median-ns=<n> all=[..]":
//   cratonvm -cp out L1W100BlockingRoundTripBench ab              (jdk-only)
//   cratonvm -cp out L1W100BlockingRoundTripBench ab --compatible
//   cratonvm -cp out L1W100BlockingRoundTripBench ab --nojit
// Its stdout without TIME lines is "ab arms=3 reps=5 ok=true".
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// park depth=4 rounds=20000 ok=true
// park depth=200 rounds=20000 ok=true
// wait depth=4 rounds=20000 ok=true
// wait depth=200 rounds=20000 ok=true
// queue depth=4 rounds=20000 ok=true
// queue depth=200 rounds=20000 ok=true
// done
// HOTSPOT_EXPECTED_END

import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.locks.LockSupport;

public class L1W100BlockingRoundTripBench {
    static final int ROUNDS = 20_000;
    static final int WARMUP = 2_000;
    static final Object LOCK = new Object();

    static volatile Thread pinger;
    static volatile Thread ponger;
    static volatile int token;
    static int waitTurn; // guarded by LOCK
    static final SynchronousQueue<Integer> toPong = new SynchronousQueue<>();
    static final SynchronousQueue<Integer> toPing = new SynchronousQueue<>();

    // The A/B arms of `ab` mode: a name and the environment the child runs
    // with (HotSpot ignores both variables, so its arms are the same VM).
    static final String[][] ARMS = {
        {"keep+reuse", "CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=1",
                "CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS=1"},
        {"keep", "CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=1",
                "CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS=0"},
        {"rebuild", "CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=0",
                "CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS=0"},
    };
    static final int AB_REPS = 5;

    public static void main(String[] args) throws Exception {
        if (args.length > 0 && args[0].equals("ab")) {
            abMain(args.length > 1 ? args[1] : null);
            return;
        }
        for (String kind : new String[] {"park", "wait", "queue"}) {
            for (int depth : new int[] {4, 200}) {
                run(kind, depth, WARMUP);
                long ns = run(kind, depth, ROUNDS);
                boolean ok = ns >= 0;
                System.out.println(kind + " depth=" + depth + " rounds=" + ROUNDS + " ok=" + ok);
                System.out.println("TIME " + kind + " depth=" + depth + " ns/roundtrip="
                        + (ns / ROUNDS));
            }
        }
        System.out.println("done");
    }

    // `ab` mode: re-runs this class (the plain mode) as a child of this VM's
    // own launcher once per arm per rep, the arms interleaved, and prints each
    // row's median per arm ("TIME ab <kind> depth=<d> arm=<arm> median-ns=..").
    // `modeFlag` (e.g. --compatible, --nojit) is passed to the child launcher
    // before -cp. Deterministic stdout: one "ab arms=3 reps=5 ok=true" line.
    static void abMain(String modeFlag) throws Exception {
        String cmd = ProcessHandle.current().info().command().orElseThrow();
        String cp = System.getProperty("java.class.path");
        java.util.Map<String, java.util.List<Long>> samples = new java.util.TreeMap<>();
        boolean ok = true;
        for (int rep = 0; rep < AB_REPS; rep++) {
            for (String[] arm : ARMS) {
                java.util.List<String> argv = new java.util.ArrayList<>();
                argv.add(cmd);
                if (modeFlag != null) {
                    argv.add(modeFlag);
                }
                argv.add("-cp");
                argv.add(cp);
                argv.add("L1W100BlockingRoundTripBench");
                ProcessBuilder pb = new ProcessBuilder(argv).redirectErrorStream(true);
                for (int i = 1; i < arm.length; i++) {
                    String[] kv = arm[i].split("=", 2);
                    pb.environment().put(kv[0], kv[1]);
                }
                Process p = pb.start();
                String out = new String(p.getInputStream().readAllBytes());
                ok &= p.waitFor() == 0 && out.contains("\ndone");
                for (String line : out.split("\\R")) {
                    // "TIME <kind> depth=<d> ns/roundtrip=<n>"
                    if (!line.startsWith("TIME ")) {
                        continue;
                    }
                    String[] f = line.split(" ");
                    String key = f[1] + " " + f[2] + " arm=" + arm[0];
                    long ns = Long.parseLong(f[3].substring(f[3].indexOf('=') + 1));
                    samples.computeIfAbsent(key, k -> new java.util.ArrayList<>()).add(ns);
                }
            }
        }
        System.out.println("ab arms=" + ARMS.length + " reps=" + AB_REPS + " ok=" + ok);
        for (java.util.Map.Entry<String, java.util.List<Long>> e : samples.entrySet()) {
            java.util.List<Long> v = new java.util.ArrayList<>(e.getValue());
            java.util.Collections.sort(v);
            System.out.println("TIME ab " + e.getKey() + " median-ns=" + v.get(v.size() / 2)
                    + " all=" + v);
        }
    }

    // Answers the elapsed nanoseconds, or -1 when a hand-off went wrong.
    static long run(String kind, int depth, int rounds) throws Exception {
        token = 0;
        waitTurn = 0;
        final long[] result = new long[] {-1};
        final boolean[] pongOk = new boolean[] {false};
        Thread pong = new Thread(() -> pongOk[0] = descend(kind, depth, rounds, false, null),
                "w100-pong");
        ponger = pong;
        pinger = Thread.currentThread();
        pong.start();
        descend(kind, depth, rounds, true, result);
        pong.join();
        return pongOk[0] ? result[0] : -1;
    }

    static boolean descend(String kind, int depth, int rounds, boolean ping, long[] result) {
        Object pinnedHere = new int[] {depth};
        if (depth > 0) {
            boolean ok = descend(kind, depth - 1, rounds, ping, result);
            return ok && ((int[]) pinnedHere)[0] == depth;
        }
        try {
            long t0 = System.nanoTime();
            boolean ok = ping ? pingLoop(kind, rounds) : pongLoop(kind, rounds);
            if (result != null) {
                result[0] = ok ? System.nanoTime() - t0 : -1;
            }
            return ok;
        } catch (InterruptedException e) {
            return false;
        }
    }

    static boolean pingLoop(String kind, int rounds) throws InterruptedException {
        for (int i = 0; i < rounds; i++) {
            switch (kind) {
                case "park":
                    token = 2 * i + 1;
                    LockSupport.unpark(ponger);
                    while (token != 2 * i + 2) {
                        LockSupport.park();
                    }
                    break;
                case "wait":
                    synchronized (LOCK) {
                        waitTurn = 2 * i + 1;
                        LOCK.notifyAll();
                        while (waitTurn != 2 * i + 2) {
                            LOCK.wait();
                        }
                    }
                    break;
                default:
                    toPong.put(i);
                    if (toPing.take() != i) {
                        return false;
                    }
                    break;
            }
        }
        return true;
    }

    static boolean pongLoop(String kind, int rounds) throws InterruptedException {
        for (int i = 0; i < rounds; i++) {
            switch (kind) {
                case "park":
                    while (token != 2 * i + 1) {
                        LockSupport.park();
                    }
                    token = 2 * i + 2;
                    LockSupport.unpark(pinger);
                    break;
                case "wait":
                    synchronized (LOCK) {
                        while (waitTurn != 2 * i + 1) {
                            LOCK.wait();
                        }
                        waitTurn = 2 * i + 2;
                        LOCK.notifyAll();
                    }
                    break;
                default:
                    if (toPong.take() != i) {
                        return false;
                    }
                    toPing.put(i);
                    break;
            }
        }
        return true;
    }
}
