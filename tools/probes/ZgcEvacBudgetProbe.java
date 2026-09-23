// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Make ZGC's evacuation budget BIND, so that its engagement counter has to
// confess.
//
// `forwarding::ZRelocationSet::select` is a prefix rule over
// `ZRelocationPolicy::max_evacuation_bytes` (64 MiB of LIVE bytes by default,
// `CRATONVM_ZGC_RELOCATE_BUDGET_MB` to change it): it walks the ranked
// candidates and the moment one would carry `live_total` past the budget it
// STOPS, counting every remaining eligible page as `deferred_for_budget`.
// `ZgcRealHeap::relocation_budget_engagement` reports that as
// `reloc_pages_deferred` / `reloc_budget_truncated_cycles` on
// `[GC] zgc-features:`.
//
// WHY A PURPOSE-BUILT PROBE, when the page that asked for the measurement
// named `G1ChurnPauseProbe 50 1800`. Because that probe cannot reach the
// budget, and reading its zero as "the constant is not binding" would be the
// vacuous green this tree keeps re-learning. Two filters stand in front of the
// budget (`ZRelocationSet::select`):
//
//   * `garbage_bytes() == 0` — a WHOLLY LIVE page is skipped: pure copy cost;
//   * `live_occupancy() >= max_live_occupancy` — default **0.25**, so a page
//     more than a quarter live is skipped as not worth the copy.
//
// `G1ChurnPauseProbe` builds its retained list densely and then churns
// elsewhere, so its low-region pages are near-wholly-live and fail BOTH
// filters. Its eligible set is nearly empty, its `live_total` never approaches
// 64 MiB, and `deferred_for_budget` is 0 at every heap size AND at a 1 MiB
// budget — which is the positive control that proves its zero says nothing
// about the constant.
//
// The arithmetic this probe is built to satisfy: to carry `live_total` past a
// 64 MiB budget through pages that are each at most 25 % live, the low region
// needs more than **256 MiB of span** at that occupancy. So: allocate a large
// span of small nodes, then null out all but one in `keepOneIn` of them, and
// churn. After the first sweep the survivors are spread thinly over the whole
// span — every page has garbage, every page is under the occupancy ceiling,
// and their live bytes sum far past the budget.
//
// Usage: {@code ZgcEvacBudgetProbe [spanMiB] [keepOneIn] [churnRounds]}
// Defaults 768 / 8 / 400: ~768 MiB of span holding ~96 MiB of live nodes at
// ~12.5 % occupancy. Run it with `--verbose:gc` and read
// `reloc_budget_truncated_cycles` on `[GC] zgc-features:`.
//
// A NON-ZERO READING IS NOT A DEFECT. It is the measurement the budget's knob
// was landed for: it says the prefix rule is truncating this workload's
// compaction, and only then is an A/B of `CRATONVM_ZGC_RELOCATE_BUDGET_MB`
// against pause p50/max measuring anything.
public final class ZgcEvacBudgetProbe {

    /** One small node: a header, a ref, a payload, and a tag. */
    static final class Node {
        Node next;
        final byte[] payload;
        int tag;

        Node(int payloadBytes, int tag) {
            this.payload = new byte[payloadBytes];
            this.tag = tag;
        }
    }

    public static void main(String[] args) {
        int spanMiB = args.length > 0 ? Integer.parseInt(args[0]) : 768;
        int keepOneIn = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 400;
        if (keepOneIn < 1) {
            keepOneIn = 1;
        }

        final int nodeBytes = 128;
        final int payloadBytes = 64;
        final int nodes = (int) (((long) spanMiB * 1024L * 1024L) / nodeBytes);

        // THE SPAN. Allocated densely and in one pass, so it lands as one
        // contiguous run of the low region rather than interleaved with the
        // churn below.
        Node[] arr = new Node[nodes];
        for (int i = 0; i < nodes; i++) {
            arr[i] = new Node(payloadBytes, i);
        }

        // THE HOLES. Everything but one in `keepOneIn` becomes unreachable in
        // place, so after the next sweep each page carries ~1/keepOneIn of its
        // capacity as live and the rest as garbage — both filters satisfied.
        int kept = 0;
        for (int i = 0; i < nodes; i++) {
            if (i % keepOneIn != 0) {
                arr[i] = null;
            } else {
                kept++;
            }
        }

        long checksum = 0;
        long start = System.nanoTime();

        // THE CHURN, to make collections happen at all. 4 MiB per round of
        // objects that die before the next cycle, plus a walk over the
        // survivors so they stay genuinely reachable and the reference-store
        // barrier runs on retained receivers.
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < (4 * 1024 * 1024) / nodeBytes; i++) {
                Node dead = new Node(payloadBytes, i);
                checksum += dead.payload.length + dead.tag;
            }
            for (int i = 0; i < nodes; i += keepOneIn * 64) {
                Node s = arr[i];
                if (s != null) {
                    s.tag = s.tag + r;
                    checksum += s.tag;
                }
            }
        }

        long wallMs = (System.nanoTime() - start) / 1_000_000L;
        System.out.println("ZgcEvacBudgetProbe"
                + " spanMiB=" + spanMiB
                + " nodes=" + nodes
                + " keepOneIn=" + keepOneIn
                + " kept=" + kept
                + " liveMiB~" + ((long) kept * nodeBytes / (1024 * 1024))
                + " rounds=" + rounds
                + " wallMs=" + wallMs
                + " checksum=" + checksum);
    }
}
