// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i23/hs (proposal {@code common-a-proposal-per-thread-handshakes}, steps 1-2,
 * opt-in {@code CRATONVM_GC_HANDSHAKE_FRAME_TRACE}): a sampler that reads another
 * thread's stack with {@code Thread.getStackTrace()} while that thread runs.
 *
 * <p>Usage: {@code GciHs23HandshakeTraceProbe <mode> <iterations> <intervalMicros> [peers]}.
 * One peer (or, for {@code exit}, {@code peers} peers one after another, each with
 * {@code iterations / peers}) runs a fixed amount of work through three named methods
 * ({@code spinA -> spinB -> spinC}), allocating a small array per iteration (the
 * interpreter's poll). The main thread samples the running peer every
 * {@code intervalMicros} until it ends. Modes:
 * <ul>
 *   <li>{@code busy}: the work only;
 *   <li>{@code block}: the peer also parks for 20 us every 4096 iterations (a peer that blocks);
 *   <li>{@code native}: the peer also runs {@code System.arraycopy} of 256 KiB and
 *       {@code Thread.yield()} every 1024 iterations (a peer that enters natives);
 *   <li>{@code gc}: the peer also calls {@code System.gc()} every 20000 iterations (a peer
 *       that is itself collecting);
 *   <li>{@code exit}: {@code peers} short-lived peers, each sampled until it exits, and once
 *       after (a peer that exits; a terminated thread's trace is empty).
 * </ul>
 *
 * <p>stdout (deterministic, HotSpot-equal): {@code HS23 <mode> checksum=<work> ok=<bool>
 * deadEmpty=<bool>}. {@code ok} is false when a sample taken while the peer was inside its
 * work from before the read to after it names no frame of this probe's classes (HotSpot
 * names one on every sample); {@code deadEmpty} is false when a joined peer's trace is not
 * empty. stderr: {@code [hs23] mode=.. samples=.. p50_us=.. p99_us=.. max_us=.. bad=..
 * spin_named=.. empty=..}, the latency of {@code getStackTrace()} per sample.
 */
public class GciHs23HandshakeTraceProbe {
    static final String SELF = "GciHs23HandshakeTraceProbe";
    static final int BUSY = 0;
    static final int BLOCK = 1;
    static final int NATIVE = 2;
    static final int GC = 3;
    static final int EXIT = 4;

    static final int[] BIG = new int[65536];
    static final int[] BIG2 = new int[65536];

    static final class Peer extends Thread {
        final int kind;
        final int iterations;
        volatile boolean inWork;
        volatile boolean started;
        long checksum;

        Peer(int kind, int iterations, String name) {
            super(name);
            this.kind = kind;
            this.iterations = iterations;
            setDaemon(true);
        }

        @Override
        public void run() {
            inWork = true;
            started = true;
            try {
                checksum = spinA(this, iterations);
            } finally {
                inWork = false;
            }
        }
    }

    static long spinA(Peer p, int n) {
        long acc = 17;
        for (int i = 0; i < n; i++) {
            acc = spinB(p, acc, i);
            if (p.kind == GC && (i % 20000) == 19999) {
                System.gc();
            }
        }
        return acc;
    }

    static long spinB(Peer p, long acc, int i) {
        int[] box = new int[4];
        box[i & 3] = i;
        acc = acc * 31 + spinC(p, box, i);
        if (p.kind == BLOCK && (i & 4095) == 4095) {
            LockSupport.parkNanos(20_000L);
        }
        return acc;
    }

    static int spinC(Peer p, int[] box, int i) {
        if (p.kind == NATIVE && (i & 1023) == 1023) {
            System.arraycopy(BIG, 0, BIG2, 0, BIG.length);
            Thread.yield();
        }
        return box[i & 3] ^ (i >>> 3);
    }

    static boolean namesSelf(StackTraceElement[] st) {
        for (StackTraceElement e : st) {
            if (e.getClassName().startsWith(SELF)) {
                return true;
            }
        }
        return false;
    }

    static boolean namesSpin(StackTraceElement[] st) {
        for (StackTraceElement e : st) {
            String m = e.getMethodName();
            if (e.getClassName().startsWith(SELF)
                    && (m.equals("spinA") || m.equals("spinB") || m.equals("spinC"))) {
                return true;
            }
        }
        return false;
    }

    static int kindOf(String mode) {
        switch (mode) {
            case "busy":
                return BUSY;
            case "block":
                return BLOCK;
            case "native":
                return NATIVE;
            case "gc":
                return GC;
            case "exit":
                return EXIT;
            default:
                throw new IllegalArgumentException("mode " + mode);
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "busy";
        int iterations = args.length > 1 ? Integer.parseInt(args[1]) : 2_000_000;
        int intervalUs = args.length > 2 ? Integer.parseInt(args[2]) : 500;
        int kind = kindOf(mode);
        int peers = kind == EXIT ? (args.length > 3 ? Integer.parseInt(args[3]) : 32) : 1;
        for (int i = 0; i < BIG.length; i++) {
            BIG[i] = i * 7;
        }
        long[] lat = new long[100_000];
        int n = 0;
        int bad = 0;
        int spinNamed = 0;
        int empty = 0;
        boolean deadEmpty = true;
        long checksum = 0;
        for (int k = 0; k < peers; k++) {
            Peer p = new Peer(kind, iterations / peers, "hs23-peer-" + k);
            p.start();
            while (!p.started) {
                Thread.onSpinWait();
            }
            while (p.isAlive()) {
                boolean before = p.inWork;
                long t0 = System.nanoTime();
                StackTraceElement[] st = p.getStackTrace();
                long dt = System.nanoTime() - t0;
                boolean after = p.inWork;
                if (n < lat.length) {
                    lat[n++] = dt;
                }
                if (st.length == 0) {
                    empty++;
                }
                if (namesSpin(st)) {
                    spinNamed++;
                }
                if (before && after && !namesSelf(st)) {
                    bad++;
                }
                LockSupport.parkNanos(intervalUs * 1000L);
            }
            p.join();
            if (p.getStackTrace().length != 0) {
                deadEmpty = false;
            }
            checksum = checksum * 31 + p.checksum;
        }
        System.out.println("HS23 " + mode + " checksum=" + checksum + " ok=" + (bad == 0)
                + " deadEmpty=" + deadEmpty);
        long[] sorted = Arrays.copyOf(lat, n);
        Arrays.sort(sorted);
        long p50 = n == 0 ? 0 : sorted[n / 2] / 1000;
        long p99 = n == 0 ? 0 : sorted[Math.min(n - 1, (int) (n * 0.99))] / 1000;
        long max = n == 0 ? 0 : sorted[n - 1] / 1000;
        System.err.println("[hs23] mode=" + mode + " samples=" + n + " p50_us=" + p50
                + " p99_us=" + p99 + " max_us=" + max + " bad=" + bad + " spin_named=" + spinNamed
                + " empty=" + empty);
    }
}
