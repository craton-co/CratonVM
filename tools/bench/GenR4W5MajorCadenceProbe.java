// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w5/concmark5 (2026-09-24): the COUNT-based measurement for
 * {@code docs/internal/gc/gengc-r4-oldgen-major-trigger-has-no-hysteresis-FIXED-20260924.md}.
 *
 * <p>Single-threaded and deterministic. A live set is parked ABOVE the 75 %
 * STW floor of the old generation: at {@code -Xmx256m} the generational old
 * generation is about 128 MiB; the default live set is 80 MiB of 64 KiB chunks
 * (about 62 %) plus a ring of 200 000 small cells (about 22 MiB with this VM's
 * object layout), together about 80 % — above the floor, below the 90 % defer
 * ceiling. Then a steady trickle of promotion: each ring cell lives
 * {@code ring} iterations, and each iteration also allocates a
 * {@code tmp}-byte young-only temporary, so a cell survives about a dozen
 * young collections — it is promoted, then dies in the old generation. About
 * 1.8 MiB of such garbage reaches the old generation per young collection,
 * well under {@code C/32} (4 MiB).
 *
 * <p>Nothing here depends on the host's speed: the young collections happen
 * at the same allocation counts on every run, so the shutdown census is a
 * function of the arguments and the policy only. Read
 * {@code [GC] major_cadence: majcad_young_decisions=Y majcad_majors=M
 * majcad_per_100_young=R majcad_back_to_back=B majcad_low_yield_5pct=L ...} and
 * {@code [GC] conc_policy: ... concpol_cycles_completed=N} from
 * {@code CRATONVM_DBG=gc-stats}. Predicted (the analysis is on the hysteresis
 * page, wave-5 block):
 * <pre>
 *   arm                                          R         B        L        N
 *   legacy, hysteresis off (the pre-09-24 run)   ~100      ~M-1     ~M       ~Y (each duplicates a major)
 *   legacy, hysteresis on                        ~10-20    ~0       ~0       ~0
 *   concurrent-first, hysteresis off (DEFAULT)   ~100      ~M-1     ~M       ~0 (starved)
 *   concurrent-first, hysteresis on              ~0        0        0        ~Y/2
 * </pre>
 * The third row is the finding: with the live set above the floor, every
 * young pause's STW collection resets the growth baseline the concurrent start
 * waits on ({@code max(C/32, (F - A)/2)}), so with less than {@code C/32} of
 * promotion per young collection the concurrent cycle never opens and
 * concurrent-first runs the legacy STW cadence. Only the STW hysteresis
 * ({@code max(C/16, (C - A)/2)} of growth before the next STW collection, and
 * {@code C/32 < C/16}) lets the concurrent start come first.
 *
 * <p>Program output must match HotSpot's byte for byte:
 * <pre>
 *   major-cadence live_mib=80 ring=200000 iters=4000000 tmp=4096 checksum=&lt;same on every VM&gt;
 * </pre>
 * Commands (build: {@code javac -d tools/bench tools/bench/GenR4W5MajorCadenceProbe.java}):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR4W5MajorCadenceProbe
 *   for arm in "" "CRATONVM_GC_OLD_TRIGGER_HYSTERESIS=1" \
 *              "CRATONVM_GC_NO_CONCURRENT_FIRST=1" \
 *              "CRATONVM_GC_NO_CONCURRENT_FIRST=1 CRATONVM_GC_OLD_TRIGGER_HYSTERESIS=1"; do
 *     env $arm CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC \
 *         -Xmx256m --nojit -cp tools/bench GenR4W5MajorCadenceProbe 2>&1 \
 *       | grep -E 'major-cadence|major_cadence:|conc_policy:|generational: minor'
 *   done
 * </pre>
 * {@code --nojit} keeps the JIT's conservative roots (and the non-moving young
 * cycles they divert to) out of the comparison, so every arm sees the same
 * young-collection sequence; drop it for the JIT-on picture. Two runs of the
 * same arm must print the same {@code majcad_} numbers; if they do not, the
 * young-collection sequence is not deterministic on this build and that is the
 * first thing to report.
 *
 * <p>Usage: {@code GenR4W5MajorCadenceProbe [liveMib] [ring] [iters] [tmp]}.
 */
public final class GenR4W5MajorCadenceProbe {
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
        final int liveMib = arg(args, 0, 80);
        final int ring = arg(args, 1, 200_000);
        final int iters = arg(args, 2, 4_000_000);
        final int tmpBytes = arg(args, 3, 4096);
        final byte[][] live = new byte[liveMib * 16][];
        for (int i = 0; i < live.length; i++) {
            live[i] = new byte[64 * 1024];
            live[i][i & 0xFFFF] = (byte) i;
        }
        final Cell[] r = new Cell[ring];
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
        }
        for (int i = 0; i < live.length; i++) {
            s += live[i][i & 0xFFFF];
        }
        System.out.println("major-cadence live_mib=" + liveMib + " ring=" + ring + " iters=" + iters
                + " tmp=" + tmpBytes + " checksum=" + s);
    }
}
