/**
 * G1 Phase-2 measurement probe: MANY remembered-set source regions pointing
 * into a small young generation.
 *
 * <h2>Why this file exists</h2>
 *
 * <p>{@code CRATONVM_G1_PARALLEL_SEED=1} spreads Phase 2 — the walk of every
 * remembered-set source region — across the evacuation workers instead of
 * running it on the driver thread alone. Wave 1 measured it on the four
 * workloads that existed ({@code G1ChurnPauseProbe}, {@code HumongousChurn}
 * twice, {@code HumongousWide}) and found it indistinguishable from the
 * default, and said why:
 * {@code orchestrator-wave-1-measurements.md} §5 — "the answer is a workload
 * with many rset sources, which none of these four are".
 *
 * <p>That is the whole point. Phase 2's cost is
 * {@code sources x region_size}, and a partition across N workers can only pay
 * when there is more than one source to partition. {@code G1ChurnPauseProbe}
 * and {@code HumongousChurn} both report {@code rset_sources=1}. This probe
 * exists to produce a large source count, so the lever has something to engage
 * on and the measurement decides something.
 *
 * <h2>The shape, and why each part of it</h2>
 *
 * <ul>
 *   <li><b>Many holders, spread WIDE — and the holder IS the bulk.</b> The
 *       remembered set is at region granularity, so a thousand edges out of one
 *       region are one source and one edge out of each of a thousand regions is
 *       a thousand sources. Each holder is therefore a 48 KiB REFERENCE ARRAY
 *       whose element 0 is the young slot and whose remaining elements are the
 *       padding: the bytes that spread the holders across regions are bytes of
 *       the holder itself. Read §"The 2026-09-21 correction" below before
 *       changing this — the first version of this probe padded with a SEPARATE
 *       byte array, and that version reported {@code rset_sources=1}.
 *   <li><b>Holders retained for the whole run</b>, so they age out of the young
 *       generation and are promoted. An edge only enters a remembered set when
 *       its SOURCE is outside the collection set — a young-to-young edge needs
 *       no entry, because young is collected whole. A probe whose holders never
 *       tenure measures nothing and reports a confident zero.
 *   <li><b>One fresh young object stored into every holder, every round.</b>
 *       This is the load-bearing store: it runs the mutator's
 *       {@code post_write_barrier_rset} and makes that holder's region a source
 *       for the young region the new object lands in.
 *   <li><b>A garbage stream between rounds</b>, so young pauses actually fire
 *       and each one has to consume the source set the round just built.
 * </ul>
 *
 * <h2>The 2026-09-21 correction: why the first shape produced ONE source</h2>
 *
 * <p>The original holder was a small object with two reference fields — a
 * {@code young} slot and a 48 KiB {@code byte[] pad} — and its comment claimed
 * that "each holder carries a padding array that pushes the next holder into
 * fresh address space; the holders end up spread over as many regions as the
 * retained volume covers". That is true of the ALLOCATION addresses, in Eden.
 * It is not true of the addresses the holders end up at, and the remembered set
 * only ever sees the latter.
 *
 * <p>G1 is a COPYING collector. A holder's final address is decided by
 * evacuation, and evacuation copies objects in the order the closure reaches
 * them: scanning the {@code live} root array copies all {@code holders}
 * elements back to back, into consecutive bytes of one destination region,
 * before it scans any of them and reaches their padding. So the padding spread
 * over ~128 regions exactly as intended and the HOLDERS — 2730 objects of ~40
 * bytes each, about 109 KiB in total — were re-packed into a single 1 MiB
 * region. The padding was a {@code byte[]}, which has no reference slots and
 * therefore can never be a remembered-set source at all.
 *
 * <p>The result was a heap with 110 Old regions, 2730 live old-to-young edges,
 * and ONE region holding all of them: {@code rset_sources=1} was the correct
 * answer to the question the heap actually posed. Verified with
 * {@code CRATONVM_G1_DBG_RSET=1}, which reported
 * {@code edges=13583 missing=0} — every edge recorded, none filtered.
 *
 * <p><b>The rule this leaves behind:</b> under a copying collector, a probe
 * cannot spread objects across regions by spreading their ALLOCATIONS. It can
 * only do it by making the objects themselves large, because a 48 KiB object is
 * 48 KiB wherever it is copied to. Padding that is a separate object gets
 * separated from the thing it was padding on the first pause.
 *
 * <h2>Reading a run</h2>
 *
 * <p>With {@code CRATONVM_GC_STATS=1}, check {@code rset_sources=} on the
 * {@code [GC] g1 cycle} line BEFORE believing any timing. A run that reports a
 * single-digit source count has not produced the shape this probe is for
 * (usually: the holders never tenured, so raise {@code rounds}, or the retained
 * set fits in too few regions, so raise {@code liveMiB}). The seed partition
 * engages only when there is more than one source region.
 *
 * <p>{@code CRATONVM_G1_DBG_RSET=1} is the other half and it is the one that
 * says WHICH of the two failures happened. Its census prints
 * {@code o2y=<edges> o2y_sources=<regions>}: the old-to-young edges the heap
 * holds, and the distinct regions they come out of. {@code o2y_sources} is the
 * number the pause's {@code rset_sources=} should equal. When they agree and
 * both are small, the probe did not build the shape; when they disagree, the
 * collector is dropping entries.
 *
 * <p>Usage: {@code G1RsetWideProbe [liveMiB] [rounds] [garbageKiBPerRound]}
 *
 * <p>The checksum is printed so a run that is faster because it lost an object
 * fails rather than scores. The 2026-09-21 reshape deliberately preserves both
 * the checksum and the {@code alive} total, so a comparison against a run of
 * the previous shape is still a comparison of the same arithmetic.
 */
public final class G1RsetWideProbe {

    public static void main(String[] args) {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 192;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int garbageKiB = args.length > 2 ? Integer.parseInt(args[2]) : 2048;

        // 48 KiB per holder puts roughly 21 holders in each 1 MiB region, so a
        // 192 MiB retained set spreads over ~192 regions and each one of them
        // becomes a remembered-set source the moment its holders take a young
        // reference. Unlike the original shape, this survives evacuation: the
        // 48 KiB is the holder, so copying the holder moves all 48 KiB of it.
        //
        // Kept WELL under the humongous threshold (half a region, 512 KiB at
        // the 1 MiB default) — a humongous holder would be allocated straight
        // into Old, never evacuated, and would measure the humongous path
        // instead of the remembered set.
        final int padBytes = 48 * 1024;
        final int padSlots = padBytes / 8;
        final int holders = (liveMiB * 1024 * 1024) / padBytes;

        // Element 0 of each holder is the young slot; 1..padSlots-1 are the
        // padding, and stay null. A reference array rather than a flat object
        // because there is no way to declare a 48 KiB flat object in Java, and
        // because the array arm is the one a real remembered-set source walk
        // spends its time in.
        final int YOUNG = 0;

        Object[][] live = new Object[holders][];
        for (int i = 0; i < holders; i++) {
            live[i] = new Object[padSlots];
        }

        long checksum = 0;
        long stores = 0;
        long start = System.nanoTime();

        for (int r = 0; r < rounds; r++) {
            // THE SOURCE-BUILDING LOOP. Every iteration is an OLD (once the
            // holders have tenured) to YOUNG reference store, in a DIFFERENT
            // region from the last one.
            for (int i = 0; i < holders; i++) {
                int[] fresh = new int[4];
                // The holder's tag is its index. The original shape carried it
                // in an `int tag` field; keeping the arithmetic identical is
                // what keeps the checksum comparable across the reshape.
                fresh[0] = i + r;
                live[i][YOUNG] = fresh;
                checksum += fresh[0];
                stores++;
            }
            // Garbage, so a young pause actually fires and has to walk the
            // source set the loop above just rebuilt.
            int chunks = (garbageKiB * 1024) / 1024;
            for (int g = 0; g < chunks; g++) {
                byte[] dead = new byte[1024];
                dead[0] = (byte) g;
                checksum += dead[0] & 0xFF;
            }
        }

        long wallMs = (System.nanoTime() - start) / 1_000_000L;

        // Read the whole retained set at the very end, so nothing above is a
        // dead store and the live set is genuinely live at the last collection.
        long alive = 0;
        for (int i = 0; i < holders; i++) {
            // `i + (byte) i` is what the original shape computed as
            // `live[i].tag + live[i].pad[0]`, where `pad[0]` had been set to
            // `(byte) tag`. Spelled out here because the pad is gone.
            alive += i + (byte) i;
            int[] y = (int[]) live[i][YOUNG];
            alive += y[0];
        }

        System.out.println("G1RsetWideProbe"
                + " live=" + liveMiB + "MiB"
                + " holders=" + holders
                + " rounds=" + rounds
                + " stores=" + stores
                + " garbageKiB=" + garbageKiB
                + " wallMs=" + wallMs
                + " alive=" + alive
                + " checksum=" + checksum);
    }
}
