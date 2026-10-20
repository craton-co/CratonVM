// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j2/cm2 ({@code CRATONVM_GC_HANDSHAKE_TARGET_KICK}, opt-in, under the opt-in
 * {@code CRATONVM_GC_HANDSHAKE_FRAME_TRACE}; proposal
 * {@code gcj-j1cm-proposal-target-only-handshake-kick}): a sampler reads the stack of one
 * target that runs a long, call-free, non-allocating interpreted loop, while
 * {@code bystanders} other threads run the same kind of loop and are never sampled.
 *
 * <p>Usage: {@code Gcj2Cm2TargetKickProbe <rounds> <intervalMicros> <bystanders>}. With the
 * j1 kick-all every sample sends every bystander's dispatch loop to its slow path once; with
 * the target kick only the target's. The bystanders count their loop iterations; more
 * iterations per second of sampling is less per-sample overhead on them.
 *
 * <p>stdout (deterministic, HotSpot-equal): {@code CM2 tk checksum=<n> ok=<bool>
 * bystanders=<k> deadEmpty=<bool>}. {@code ok} is false when a sample taken while the target
 * was inside its work from before the read to after it names no {@code spin*} frame. stderr:
 * {@code [cm2tk] samples=.. p50_us=.. p99_us=.. bad=.. bystander_iters_per_ms=..}.
 */
public class Gcj2Cm2TargetKickProbe {
    static final String SELF = "Gcj2Cm2TargetKickProbe";
    static volatile boolean stop;

    static final class Target extends Thread {
        final int rounds;
        volatile boolean inWork;
        volatile boolean started;
        long checksum;

        Target(int rounds) {
            super("cm2-target");
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

    static final class Bystander extends Thread {
        volatile boolean started;
        long iters;

        Bystander(int i) {
            super("cm2-bystander-" + i);
            setDaemon(true);
        }

        @Override
        public void run() {
            started = true;
            long n = 0;
            long acc = 7;
            while (!stop) {
                for (int i = 0; i < 1_000; i++) {
                    acc = acc * 31 + i;
                }
                n++;
            }
            iters = n + (acc & 0);
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
        int k = args.length > 2 ? Integer.parseInt(args[2]) : 16;
        Bystander[] bys = new Bystander[k];
        for (int i = 0; i < k; i++) {
            bys[i] = new Bystander(i);
            bys[i].start();
        }
        for (Bystander b : bys) {
            while (!b.started) {
                Thread.onSpinWait();
            }
        }
        long[] lat = new long[200_000];
        int n = 0;
        int bad = 0;
        Target t = new Target(rounds);
        long w0 = System.nanoTime();
        t.start();
        while (!t.started) {
            Thread.onSpinWait();
        }
        while (t.isAlive()) {
            boolean before = t.inWork;
            long t0 = System.nanoTime();
            StackTraceElement[] st = t.getStackTrace();
            long t1 = System.nanoTime();
            boolean after = t.inWork;
            if (n < lat.length) {
                lat[n++] = (t1 - t0) / 1000;
            }
            if (before && after && !namesSpin(st)) {
                bad++;
            }
            long until = System.nanoTime() + intervalUs * 1000L;
            while (System.nanoTime() < until) {
                Thread.onSpinWait();
            }
        }
        t.join();
        stop = true;
        long total = 0;
        for (Bystander b : bys) {
            b.join();
            total += b.iters;
        }
        long wallMs = Math.max(1, (System.nanoTime() - w0) / 1_000_000);
        boolean deadEmpty = t.getStackTrace().length == 0;
        java.util.Arrays.sort(lat, 0, n);
        long p50 = n == 0 ? 0 : lat[n / 2];
        long p99 = n == 0 ? 0 : lat[Math.min(n - 1, (int) (n * 0.99))];
        System.err.println("[cm2tk] samples=" + n + " p50_us=" + p50 + " p99_us=" + p99
                + " bad=" + bad + " bystander_iters_per_ms=" + (total / wallMs));
        System.out.println("CM2 tk checksum=" + t.checksum + " ok=" + (bad == 0)
                + " bystanders=" + k + " deadEmpty=" + deadEmpty);
    }
}
