// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j3/x2: fail-first probe of the STW undiscovered-references probe's silent
// budget cutoff (`gcj-j2x1-wave-j2-default-flips-...` item (ii); the probe is on the
// DEFAULT Generational path since the j2 flip of CRATONVM_STW_UNDISCOVERED_REFS_STRONG).
//
// The CratonVM probe walks from EVERY registered finalizable -- live ones included --
// and gives up past 1 << 20 visited objects (gc_and_alloc.rs `STW_PROBE_WALK_BUDGET`),
// after which the pause runs as without the switch. HotSpot has no budget: a
// Reference that only a dead finalizable X reaches is an ordinary object in the
// keep-alive phase, so its referent is kept and X.finalize() reads it.
//
// Mode (first argument): `big` -- a LIVE finalizable (static) holds an Object[] of
// 1.2M objects, so the walk passes the budget; `small` -- the same with 1000 (the
// control: the walk fits). Both: X (dead) -> WeakReference W -> payload Y, nothing
// else reaches Y.
//
// stdout (deterministic; HotSpot 25 -XX:+UseSerialGC -Xmx512m), both modes:
//   finalized-once: true
//   undiscovered-kept: true
// stderr: the mode, the finalized count, the System.gc() CPU time.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.lang.ref.WeakReference;

public class Gcj3X2StwProbeBudgetProbe {
    static final int LEN = 256;
    static volatile int finalized;
    static volatile Fin resurrected;
    static LiveFin live;

    static final class LiveFin {
        Object[] graph;

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {}
    }

    static final class Fin {
        WeakReference<long[]> weak;

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            resurrected = this;
            finalized = finalized + 1;
        }
    }

    static long[] payload() {
        long[] a = new long[LEN];
        for (int i = 0; i < LEN; i++) {
            a[i] = i * 31L + 7;
        }
        return a;
    }

    static boolean intact(long[] a) {
        if (a == null || a.length != LEN) {
            return false;
        }
        for (int i = 0; i < LEN; i++) {
            if (a[i] != i * 31L + 7) {
                return false;
            }
        }
        return true;
    }

    static void buildDead() {
        Fin x = new Fin();
        x.weak = new WeakReference<>(payload());
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "big";
        int n = mode.equals("small") ? 1000 : 1_200_000;
        LiveFin l = new LiveFin();
        l.graph = new Object[n];
        for (int i = 0; i < n; i++) {
            l.graph[i] = new Object();
        }
        live = l;
        System.gc();
        buildDead();
        ThreadMXBean tmx = ManagementFactory.getThreadMXBean();
        long cpu0 = tmx.getCurrentThreadCpuTime();
        System.gc();
        long cpu1 = tmx.getCurrentThreadCpuTime();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (finalized == 0 && System.nanoTime() < deadline) {
            System.runFinalization();
            Thread.sleep(10);
        }
        Fin back = resurrected;
        System.out.println("finalized-once: " + (finalized == 1));
        System.out.println("undiscovered-kept: " + (back != null && intact(back.weak.get())));
        System.err.println("[x2-budget] mode=" + mode + " n=" + n + " finalized=" + finalized
                + " gc-cpu-us=" + (cpu1 - cpu0) / 1000 + " live=" + (live.graph.length == n));
    }
}
