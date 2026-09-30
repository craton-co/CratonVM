// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w4/young4 (2026-09-24): SURVIVOR OVERFLOW / premature promotion.
 *
 * <p>HotSpot overflows a survivor space when a young collection's survivors do
 * not fit it, and tenures the overflow early. CratonVM's generational young
 * generation has no survivor space to overflow — to-space is as large as
 * from-space — so its equivalent is the PROMOTE-ON-PRESSURE arm: a moving
 * cycle that sees more than 75% of a half-full nursery survive arms the NEXT
 * young cycle to tenure every survivor regardless of age. This probe drives
 * that arm on purpose and then checks the thing that used to go wrong with it.
 *
 * <p>Each wave:
 * <ol>
 *   <li>SPIKE — retains {@code retainMiB} of 256-byte records, interleaved with
 *       garbage, so several consecutive young cycles see very high survival
 *       (arms the promote-on-pressure flag);</li>
 *   <li>MEDIUM-LIVED — churns a sliding window of recent records that each die
 *       a few thousand allocations after birth: exactly the objects an arm left
 *       over from the spike tenures prematurely (they reach old gen and die
 *       there, which only a major collection reclaims);</li>
 *   <li>DROP — verifies and releases the spike set.</li>
 * </ol>
 * Every record's payload is re-derived and checked, so a mis-copy is a FAIL.
 *
 * <p>Expected output (defaults; HotSpot 25, any collector):
 * <pre>
 *   PASS overflow waves=6 retainMiB=40 checksum=1181091290535902592 corrupt=0
 *   (args 3 40)  PASS overflow waves=3 retainMiB=40 checksum=9176405803722710720 corrupt=0
 * </pre>
 * What to read on CratonVM
 * ({@code CRATONVM_DBG=gc-stats}):
 * <ul>
 *   <li>{@code [GC] promote_pressure:} — {@code promote_pressure_armed >= 1}
 *       (the probe drove the arm), {@code promote_pressure_consumed_stale=0}
 *       (gen r4w4: an arm is never carried across a non-moving cycle any more),
 *       and {@code consumed + dropped_on_divert} equal to {@code armed} or one
 *       less (an arm still pending at exit);</li>
 *   <li>the {@code Tenured Gen} peak / {@code [GC] generational: major=} —
 *       premature promotion shows as old-gen growth during the MEDIUM-LIVED
 *       phase; compare against HotSpot's {@code -Xlog:gc*} tenuring.</li>
 * </ul>
 * Commands:
 * <pre>
 *   java -Xmx256m -Xlog:gc -cp tools/bench GenR4W4SurvivorOverflowProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4SurvivorOverflowProbe
 *   CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4SurvivorOverflowProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m --nojit -cp tools/bench GenR4W4SurvivorOverflowProbe 3 40
 * </pre>
 * Optional args: {@code waves retainMiB}. Exit status 1 on FAIL.
 */
public final class GenR4W4SurvivorOverflowProbe {
    static final class Rec {
        final long id;
        final long[] data;
        Rec next;

        Rec(long id) {
            this.id = id;
            this.data = new long[26]; // ~208 B + headers: ~256 B per record
            for (int k = 0; k < data.length; k++) {
                data[k] = id * (k + 7);
            }
        }

        boolean intact() {
            for (int k = 0; k < data.length; k++) {
                if (data[k] != id * (k + 7)) {
                    return false;
                }
            }
            return true;
        }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        final int waves = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        final int retainMiB = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        final int spikeRecords = retainMiB * 4096; // 256 B each
        final int window = 4096;
        final int mediumIters = 2_000_000;

        long checksum = 0;
        long corrupt = 0;
        long nextId = 1;
        for (int w = 0; w < waves; w++) {
            // 1. SPIKE: a long-lived set built inside the nursery.
            Rec head = null;
            for (int i = 0; i < spikeRecords; i++) {
                final Rec r = new Rec(nextId++);
                r.next = head;
                head = r;
                if ((i & 3) == 0) {
                    sink = new byte[64]; // a little garbage between survivors
                }
            }
            // 2. MEDIUM-LIVED: each record dies `window` allocations later.
            final Rec[] ring = new Rec[window];
            for (int i = 0; i < mediumIters; i++) {
                final int slot = i % window;
                final Rec old = ring[slot];
                if (old != null) {
                    if (!old.intact()) {
                        corrupt++;
                    }
                    checksum = checksum * 31 + old.id;
                }
                ring[slot] = new Rec(nextId++);
            }
            for (Rec r : ring) {
                if (!r.intact()) {
                    corrupt++;
                }
                checksum = checksum * 31 + r.id;
            }
            // 3. DROP: verify the spike set, then let it die.
            for (Rec r = head; r != null; r = r.next) {
                if (!r.intact()) {
                    corrupt++;
                }
                checksum = checksum * 17 + r.data[3];
            }
            head = null;
        }
        sink = null;
        final String verdict = corrupt == 0 ? "PASS" : "FAIL";
        System.out.println(verdict + " overflow waves=" + waves + " retainMiB=" + retainMiB
                + " checksum=" + checksum + " corrupt=" + corrupt);
        if (corrupt != 0) {
            System.exit(1);
        }
    }
}
