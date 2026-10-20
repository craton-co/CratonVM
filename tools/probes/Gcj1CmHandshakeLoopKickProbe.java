// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j1/cm (step P4/ph of {@code common-a-proposal-per-thread-handshakes},
 * {@code CRATONVM_GC_HANDSHAKE_LOOP_KICK} under the opt-in
 * {@code CRATONVM_GC_HANDSHAKE_FRAME_TRACE}): a sampler reads the stack of a peer
 * that runs a long, call-free, NON-ALLOCATING interpreted loop.
 *
 * <p>Usage: {@code Gcj1CmHandshakeLoopKickProbe <rounds> <intervalMicros>}. The peer
 * runs {@code spinA -> spinB -> spinC}; {@code spinC} is a 200 000-iteration
 * arithmetic loop with no call and no allocation, so the only poll an interpreted
 * peer reaches is its dispatch loop's poll word. The main thread samples the peer's
 * {@code getStackTrace()} every {@code intervalMicros} until it ends.
 *
 * <p>stdout (deterministic, HotSpot-equal): {@code CM1 kick checksum=<n> ok=<bool>
 * deadEmpty=<bool>}. {@code ok} is false when a sample taken while the peer was
 * inside its work from before the read to after it names no {@code spin*} frame.
 * stderr: {@code [cm1] samples=.. p50_us=.. p99_us=.. max_us=.. bad=.. empty=..}.
 */
public class Gcj1CmHandshakeLoopKickProbe {
    static final String SELF = "Gcj1CmHandshakeLoopKickProbe";

    static final class Peer extends Thread {
        final int rounds;
        volatile boolean inWork;
        volatile boolean started;
        long checksum;

        Peer(int rounds) {
            super("cm1-peer");
            this.rounds = rounds;
            setDaemon(true);
        }

        @Override
        public void run() {
            inWork = true;
            started = true;
            try {
                checksum = spinA(rounds);
            } finally {
                inWork = false;
            }
        }
    }

    static long spinA(int rounds) {
        long acc = 17;
        for (int r = 0; r < rounds; r++) {
            acc = spinB(acc, r);
        }
        return acc;
    }

    static long spinB(long acc, int r) {
        return spinC(acc, r);
    }

    static long spinC(long acc, int r) {
        for (int i = 0; i < 200_000; i++) {
            acc = acc * 31 + (i ^ r);
        }
        return acc;
    }

    static boolean namesSpin(StackTraceElement[] st) {
        for (StackTraceElement e : st) {
            String m = e.getMethodName();
            if (e.getClassName().equals(SELF)
                    && (m.equals("spinA") || m.equals("spinB") || m.equals("spinC"))) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        int intervalUs = args.length > 1 ? Integer.parseInt(args[1]) : 500;
        long[] lat = new long[200_000];
        int n = 0;
        int bad = 0;
        int empty = 0;
        Peer p = new Peer(rounds);
        p.start();
        while (!p.started) {
            Thread.onSpinWait();
        }
        while (p.isAlive()) {
            boolean before = p.inWork;
            long t0 = System.nanoTime();
            StackTraceElement[] st = p.getStackTrace();
            long t1 = System.nanoTime();
            boolean after = p.inWork;
            if (n < lat.length) {
                lat[n++] = (t1 - t0) / 1000;
            }
            if (st.length == 0) {
                empty++;
            }
            if (before && after && !namesSpin(st)) {
                bad++;
            }
            long until = System.nanoTime() + intervalUs * 1000L;
            while (System.nanoTime() < until) {
                Thread.onSpinWait();
            }
        }
        p.join();
        boolean deadEmpty = p.getStackTrace().length == 0;
        java.util.Arrays.sort(lat, 0, n);
        long p50 = n == 0 ? 0 : lat[n / 2];
        long p99 = n == 0 ? 0 : lat[Math.min(n - 1, (int) (n * 0.99))];
        long max = n == 0 ? 0 : lat[n - 1];
        System.err.println("[cm1] samples=" + n + " p50_us=" + p50 + " p99_us=" + p99
                + " max_us=" + max + " bad=" + bad + " empty=" + empty);
        System.out.println("CM1 kick checksum=" + p.checksum + " ok=" + (bad == 0)
                + " deadEmpty=" + deadEmpty);
    }
}
