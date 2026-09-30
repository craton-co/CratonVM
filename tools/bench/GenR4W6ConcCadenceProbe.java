// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w6/concsvc6 (2026-09-24): the concurrent old-gen cycle's CADENCE, with
 * and without the service thread ({@code CRATONVM_GEN_CONC_SERVICE_THREAD}).
 *
 * <p>Single-threaded and deterministic. A steady old-generation churn over a
 * live set of about 40 % of the old generation (at {@code -Xmx256m} the
 * generational old generation is about 128 MiB):
 * <ul>
 *   <li>{@code liveMib} MiB of 64 KiB chunks, held to the end (40 MiB, ~31 %);</li>
 *   <li>a ring of {@code ring} small cells, each replaced {@code ring}
 *       iterations after it was made (~10 MiB live, ~8 %). A cell outlives
 *       several young collections, so it is PROMOTED and then dies in the old
 *       generation: that is the old-gen churn the concurrent cycle collects;</li>
 *   <li>every {@code slabEvery} iterations one {@code slabKib} KiB array into a
 *       ring of {@code slabRing} (4 MiB live, ~3 %): large arrays are allocated
 *       in the old generation DIRECTLY on this VM, with no young collection
 *       involved, which only the service's growth signal
 *       ({@code concdrv_growth_signals}) sees;</li>
 *   <li>a {@code tmp}-byte young-only temporary per iteration, which paces the
 *       young collections.</li>
 * </ul>
 *
 * <p>Output (the same on every VM, HotSpot included; computed in closed form
 * for the defaults):
 * <pre>
 *   conc-cadence live_mib=40 ring=100000 iters=4000000 tmp=4096 slab_kib=256 slab_every=2000 slab_ring=16 checksum=235755644852624
 * </pre>
 * The checksum is: cells {@code sum_{m<3900000} (31m + 48) = 235755126750000},
 * temporaries {@code sum_{i<4000000} (byte) i = -2000000}, slabs
 * {@code sum_{e=16}^{1999} ((byte)(e-16) + 262144) = 520094752} and the live
 * chunks {@code sum_{i<640} (byte) i = 7872}.
 *
 * <p>What to read (count-based; nothing here depends on the host's speed
 * except {@code concdrv_trigger_to_start_*}, which is wall-clock and labelled
 * {@code _us}). From {@code CRATONVM_DBG=gc-stats}:
 * <ul>
 *   <li>{@code [GC] conc_driver: ... concdrv_cycles_started=S
 *       concdrv_cycles_completed=C concdrv_trigger_to_start_n=N
 *       concdrv_trigger_to_start_avg_us=A concdrv_trigger_to_start_max_us=M
 *       ...} (and, with the service, {@code concdrv_service_attached=true},
 *       {@code concdrv_handoffs}, {@code concdrv_growth_signals},
 *       {@code concdrv_service_cycles_completed});</li>
 *   <li>{@code [GC] major_cadence: ... majcad_majors=J
 *       majcad_fallback_conc_too_slow=F majcad_fallback_cycle_open=...
 *       majcad_fallback_start_late=... majcad_other=O ...}: STW majors, split
 *       into "the concurrent cycle was too slow" and the rest.</li>
 * </ul>
 * Expected: {@code C >= 1} in both arms; {@code F} near 0 in both (the live
 * set is well under the 75 % STW floor, so a STW major means the cycle did not
 * keep up); with the service, {@code concdrv_service_cycles_completed} close
 * to {@code C} and {@code concdrv_growth_signals >= 1}. The A/B the
 * orchestrator runs is {@code C}, {@code F} and the wall time of the whole
 * run, flag on vs off, interleaved.
 *
 * <p>Commands (build: {@code javac -d tools/bench tools/bench/GenR4W6ConcCadenceProbe.java}):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR4W6ConcCadenceProbe
 *   for arm in "" "CRATONVM_GEN_CONC_SERVICE_THREAD=1"; do
 *     env $arm CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC \
 *         -Xmx256m --nojit -cp tools/bench GenR4W6ConcCadenceProbe 2>&1 \
 *       | grep -E 'conc-cadence|conc_driver:|major_cadence:|conc_policy:|generational: minor'
 *   done
 * </pre>
 * {@code --nojit} keeps the JIT's conservative roots (and the non-moving young
 * cycles they divert to) out of the comparison; drop it for the JIT-on
 * picture.
 *
 * <p>Usage: {@code GenR4W6ConcCadenceProbe [liveMib] [ring] [iters] [tmp]
 * [slabKib] [slabEvery] [slabRing]}.
 */
public final class GenR4W6ConcCadenceProbe {
    static final class Cell {
        final long v;
        final byte[] pad;

        Cell(long v) {
            this.v = v;
            this.pad = new byte[48];
        }
    }

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    public static void main(String[] args) {
        final int liveMib = arg(args, 0, 40);
        final int ring = arg(args, 1, 100_000);
        final int iters = arg(args, 2, 4_000_000);
        final int tmpBytes = arg(args, 3, 4096);
        final int slabKib = arg(args, 4, 256);
        final int slabEvery = arg(args, 5, 2000);
        final int slabRing = arg(args, 6, 16);

        final byte[][] live = new byte[liveMib * 16][];
        for (int i = 0; i < live.length; i++) {
            live[i] = new byte[64 * 1024];
            live[i][i & 0xFFFF] = (byte) i;
        }
        final Cell[] r = new Cell[ring];
        final byte[][] slabs = new byte[slabRing][];
        long s = 0;
        for (int i = 0; i < iters; i++) {
            int k = i % ring;
            Cell old = r[k];
            if (old != null) {
                s += old.v + old.pad.length;
            }
            r[k] = new Cell(i * 31L);
            byte[] tmp = new byte[tmpBytes];
            tmp[i % tmpBytes] = (byte) i;
            s += tmp[i % tmpBytes];
            if (i % slabEvery == 0) {
                int e = i / slabEvery;
                int j = e % slabRing;
                byte[] prev = slabs[j];
                if (prev != null) {
                    s += prev[0] + prev.length;
                }
                byte[] slab = new byte[slabKib * 1024];
                slab[0] = (byte) e;
                slabs[j] = slab;
            }
        }
        for (int i = 0; i < live.length; i++) {
            s += live[i][i & 0xFFFF];
        }
        System.out.println("conc-cadence live_mib=" + liveMib + " ring=" + ring + " iters=" + iters
                + " tmp=" + tmpBytes + " slab_kib=" + slabKib + " slab_every=" + slabEvery
                + " slab_ring=" + slabRing + " checksum=" + s);
    }
}
