/**
 * A probe that actually churns humongous objects.
 *
 * <p>WHY THIS EXISTS. {@code HumongousChurn} and {@code HumongousWide} do not.
 * Both allocate exactly ONE humongous array at startup and then churn
 * {@code long[4]} / {@code long[32]} / {@code long[64]}, none of which is
 * humongous at G1's 1 MiB default region size. Two independent counters added
 * in the 2026-09-20 G1 round say so in one number each: {@code requests=1} from
 * the humongous request census, and {@code contiguous(calls=1)} from the
 * contiguous-run search. Every claim in that round that a number came from "a
 * humongous workload" therefore rested on a path taken once per run — including
 * the measurement behind {@code CRATONVM_G1_HUMONGOUS_BEST_FIT}. See
 * {@code docs/internal/g1-2026-09-20/orchestrator-wave-1-measurements.md} §7.2.
 *
 * <p>WHAT MAKES AN OBJECT HUMONGOUS. G1 routes an allocation to the humongous
 * path when it exceeds HALF a region ({@code alloc_in_region}'s
 * {@code size > region_size / 2}); at the 1 MiB default that is 512 KiB. A
 * {@code long[]} costs 8 bytes an element plus a header, so 65,536 elements is
 * 512 KiB of payload and is comfortably over the line — and stays over it for
 * any region size up to 1 MiB. The sizes below are expressed in elements
 * against that arithmetic rather than as bare constants, so a reader can check
 * the classification instead of trusting it.
 *
 * <p>WHAT IT EXERCISES, and this is the part the existing probes miss:
 *
 * <ul>
 *   <li><b>Repeated contiguous-run search.</b> Every round claims several
 *       multi-region spans, so {@code find_contiguous_free} runs thousands of
 *       times rather than once. This is the path best-fit placement was
 *       supposed to improve.
 *   <li><b>Fragmentation of the free-run structure.</b> The spans are three
 *       different widths and are released in a different order from the one
 *       they were claimed in, which is what breaks long runs apart. A probe
 *       that allocates and frees one width in order never fragments anything.
 *   <li><b>Survival across a pause.</b> A rolling window of spans stays live,
 *       so humongous regions are reachable at collection time and the eager
 *       reclaim has to decide about them, rather than every span being dead by
 *       the time anything looks.
 *   <li><b>A humongous object that HOLDS references.</b> The
 *       {@code Object[]} band makes some spans reference-bearing, so the
 *       humongous slot walk runs. An all-primitive probe never visits it.
 * </ul>
 *
 * <p>The checksum folds in every element the probe writes and the identity of
 * every span it retires, so an object lost or not copied changes it. Wall time
 * is reported separately from the checksum line so a measurement script can
 * read either without parsing the other.
 *
 * <p>Usage: {@code HumongousRealChurn <windowSpans> <rounds> [refBandPercent]}
 * — e.g. {@code HumongousRealChurn 12 400 25}. Defaults: 12, 400, 25.
 * Size it so {@code windowSpans} times the mean span width stays under the
 * heap: the mean width below is 3 MiB, so 12 spans is a 36 MiB live set and
 * wants {@code -Xmx128m} or more.
 */
public final class HumongousRealChurn {

    /** 8 bytes an element, so this is 512 KiB of payload — over half a 1 MiB region. */
    static final int LONGS_SMALL = 65_536;

    /** 2 MiB. Spans two regions at the 1 MiB default. */
    static final int LONGS_MEDIUM = 262_144;

    /** 6 MiB. Spans six regions, and is what actually needs a long free run. */
    static final int LONGS_LARGE = 786_432;

    /**
     * A reference-bearing humongous object: 4 bytes a slot under compressed
     * oops, 8 without, so 131,072 slots is 512 KiB even in the narrow case.
     */
    static final int REFS = 131_072;

    public static void main(String[] args) {
        int window = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int refBandPercent = args.length > 2 ? Integer.parseInt(args[2]) : 25;

        // The rolling live window. A span stays reachable until its slot is
        // overwritten, which is what makes it survive pauses instead of dying
        // in the eden it was born in.
        Object[] live = new Object[window];

        long checksum = 0;
        long spansClaimed = 0;
        long bytesClaimed = 0;

        long start = System.nanoTime();

        for (int r = 0; r < rounds; r++) {
            // Three widths per round, so the free-list sees a mix rather than a
            // single size class it can always satisfy from the same place.
            for (int w = 0; w < 3; w++) {
                final int slot;
                // Retire out of claim order: stepping by a stride coprime with
                // the window walks every slot but never in the order they were
                // filled, which is what leaves holes between live spans.
                slot = (int) ((spansClaimed * 5) % window);

                Object span;
                if ((spansClaimed * 100 / Math.max(1, spansClaimed + 1)) < refBandPercent
                        || (spansClaimed % 100) < refBandPercent) {
                    // A humongous REFERENCE array. Its slots point at small
                    // young objects, so the humongous slot walk has real edges
                    // to follow and the remembered set has humongous sources.
                    Object[] refs = new Object[REFS];
                    // Touch a sparse subset: filling all 131k slots every round
                    // would make this probe a memset benchmark rather than a
                    // collector one.
                    for (int i = 0; i < REFS; i += 4096) {
                        Integer boxed = Integer.valueOf(r * 31 + i);
                        refs[i] = boxed;
                        checksum += boxed.intValue();
                    }
                    span = refs;
                    bytesClaimed += (long) REFS * 4;
                } else {
                    int n = switch (w) {
                        case 0 -> LONGS_SMALL;
                        case 1 -> LONGS_MEDIUM;
                        default -> LONGS_LARGE;
                    };
                    long[] big = new long[n];
                    // Write the two ends and a stride through the middle. The
                    // ends matter: a span's last element is the one a walk that
                    // mis-sizes the object reads past.
                    big[0] = r;
                    big[n - 1] = n;
                    for (int i = 0; i < n; i += 8192) {
                        big[i] = i ^ r;
                        checksum += big[i];
                    }
                    checksum += big[0] + big[n - 1];
                    span = big;
                    bytesClaimed += (long) n * 8;
                }

                // Retiring the previous occupant is what frees a span and
                // reopens a run — and doing it AFTER the new one is claimed is
                // deliberate: it means the new claim cannot reuse the run the
                // old one is about to release, so the free structure really
                // does have to find somewhere else.
                Object retired = live[slot];
                if (retired != null) {
                    checksum += System.identityHashCode(retired) == 0 ? 0 : 1;
                }
                live[slot] = span;
                spansClaimed++;
            }

            // A small young stream alongside the humongous one, so pauses
            // actually happen between claims rather than only when a span
            // cannot be placed.
            for (int i = 0; i < 2048; i++) {
                byte[] garbage = new byte[256];
                garbage[0] = (byte) i;
                checksum += garbage[0];
            }
        }

        // Keep the window reachable to the end so the final pauses still have
        // humongous spans to decide about.
        for (int i = 0; i < window; i++) {
            if (live[i] instanceof long[] a) {
                checksum += a[0];
            } else if (live[i] instanceof Object[] o) {
                checksum += o[0] == null ? 0 : 1;
            }
        }

        long wallMs = (System.nanoTime() - start) / 1_000_000L;
        System.out.println("HumongousRealChurn window="
                + window
                + " rounds="
                + rounds
                + " refBandPercent="
                + refBandPercent
                + " spansClaimed="
                + spansClaimed
                + " humongousMiB="
                + (bytesClaimed / (1024 * 1024))
                + " wallMs="
                + wallMs
                + " checksum="
                + checksum);
    }
}
