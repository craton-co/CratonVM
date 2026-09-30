/**
 * G1 Phase-2 measurement probe: source regions that are MOSTLY CLEAN and FULL
 * OF SMALL OBJECTS — the shape the block-offset table exists for.
 *
 * <h2>Why this file exists, and why {@code G1RsetWideProbe} could not serve</h2>
 *
 * <p>The block-offset table
 * ({@code docs/internal/g1-2026-09-20/lane-a-block-offset-table.md},
 * {@code CRATONVM_G1_BLOCK_OFFSETS}) removes the LINEAR STEP from the
 * remembered-set source walk: the header read, the plausibility screen and the
 * {@code object_total_size} the walk pays for every object it steps over on its
 * way to a dirty card. So the size of the effect is
 * <b>objects stepped over per pause</b>, and a workload can only show it when
 * two things are true at once:
 *
 * <ul>
 *   <li><b>the source regions hold MANY objects</b> — the term is per object,
 *       not per byte, so one 48 KiB object and fifteen hundred 32-byte ones
 *       cost the same bytes and differ by three orders of magnitude in what the
 *       table removes;
 *   <li><b>most of their cards are CLEAN</b> — the walk can only skip what the
 *       card screen was going to reject anyway, so a region every card of which
 *       is dirty offers the table nothing however many objects are in it.
 * </ul>
 *
 * <p>{@code G1RsetWideProbe} was reshaped in wave 3 to produce many
 * remembered-set sources and it does ({@code rset_sources=132}). But its holder
 * is a 48 KiB reference array with ONE non-null element, so a 1 MiB source
 * region holds about seventeen objects and its measured skip rate is 0.82%.
 * Seventeen objects is not a linear term. That probe is the right one for the
 * per-SLOT cost of a wide holder and the wrong one for the per-OBJECT cost of a
 * walk, and the two were conflated because both are called "the Phase 2 walk".
 *
 * <h2>The shape, and why each part of it</h2>
 *
 * <ul>
 *   <li><b>A retained set of SMALL objects.</b> {@code int[4]} is about forty
 *       bytes, so a 1 MiB region holds twenty-five thousand of them. These are
 *       the objects the walk steps over, and they have no reference slots at
 *       all — which is the point: they cost the walk nothing but the linear
 *       step, so what the arms differ by is the linear step and not a scan.
 *   <li><b>One in every {@code HOLDER_STRIDE} of them is a HOLDER</b>, an
 *       {@code Object[2]} that takes a fresh young reference every round. That
 *       is what makes its region a remembered-set source, and at a stride of
 *       500 it leaves roughly fifty dirty cards in a region of two thousand.
 *   <li><b>The SAME holders every round.</b> G1's card table is additive — a
 *       card goes clean only at region reset, which never happens to a tenured
 *       holder region — so a probe that rotated which objects it stored into
 *       would saturate every card within a few dozen rounds and measure
 *       nothing. Storing into a fixed subset keeps the dirty set sparse without
 *       needing {@code CRATONVM_G1_CARD_CLEAN}, so this measures the DEFAULT
 *       configuration rather than one that needs two levers to show one effect.
 *   <li><b>Holders and filler interleaved in one retained array.</b> Under a
 *       copying collector a probe cannot spread objects across regions by
 *       spreading their allocations — evacuation copies in the order the
 *       closure reaches them, so the elements of one root array land back to
 *       back whatever their allocation addresses were. That is the rule
 *       {@code G1RsetWideProbe}'s "2026-09-21 correction" section establishes,
 *       and interleaving is how this probe uses it rather than fights it: the
 *       holders end up evenly spread through the filler because that is the
 *       order the root array lists them in.
 *   <li><b>A garbage stream between rounds</b>, so young pauses actually fire
 *       and each has to consume the source set the round just built.
 * </ul>
 *
 * <h2>Reading a run</h2>
 *
 * <p>With {@code CRATONVM_GC_STATS=1}:
 *
 * <ul>
 *   <li>{@code rset_sources=} on the {@code [GC] g1 cycle} line — this probe is
 *       meaningless below a few dozen. If it is small the holders did not
 *       tenure (raise {@code rounds}) or the retained set fits in too few
 *       regions (raise {@code liveMiB}).
 *   <li>{@code [GC] g1 card-clean: bytes_scanned= bytes_skipped= skip_rate=} —
 *       the skip rate should be very HIGH here, the opposite of
 *       {@code G1RsetWideProbe}'s 0.82%. A high skip rate is the precondition
 *       for the block-offset table having anything to do: those skipped bytes
 *       are objects the walk sized and rejected, and the table's whole job is
 *       to not size them.
 *   <li>{@code [GC] g1 block-offsets: jumps= bytes_jumped= entries_refused=} —
 *       the table's own engagement. {@code entries_refused} must be ZERO;
 *       anything else means the walk was handed an address that is not an
 *       object header and refused it, and the lever must go back to off.
 * </ul>
 *
 * <p>Usage:
 * {@code G1BlockOffsetProbe [liveMiB] [rounds] [garbageKiBPerRound]}
 *
 * <p>The checksum is printed so a run that is faster because it lost an object
 * fails rather than scores — which for this lever is the failure that matters:
 * a walk entered above an object does not crash, it silently drops that
 * object's remembered-set edge.
 */
public final class G1BlockOffsetProbe {

    /** One in this many retained objects is a holder that takes a young store. */
    private static final int HOLDER_STRIDE = 500;

    /** Approximate retained bytes per element, for sizing the array. */
    private static final int BYTES_PER_ELEMENT = 40;

    public static void main(String[] args) {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 128;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 600;
        int garbageKiB = args.length > 2 ? Integer.parseInt(args[2]) : 4096;

        final int n = (liveMiB * 1024 * 1024) / BYTES_PER_ELEMENT;

        Object[] live = new Object[n];
        int holderCount = 0;
        for (int i = 0; i < n; i++) {
            if (i % HOLDER_STRIDE == 0) {
                live[i] = new Object[2];
                holderCount++;
            } else {
                // No reference slots: this object costs the source walk exactly
                // the linear step and nothing else, which is what makes the
                // two arms differ by the linear step alone.
                int[] filler = new int[4];
                filler[0] = i;
                live[i] = filler;
            }
        }

        long checksum = 0;
        long stores = 0;
        long start = System.nanoTime();

        for (int r = 0; r < rounds; r++) {
            // THE SOURCE-BUILDING LOOP. A fixed, sparse subset, so the dirty
            // card set stays sparse for the whole run.
            for (int i = 0; i < n; i += HOLDER_STRIDE) {
                Object[] holder = (Object[]) live[i];
                int[] fresh = new int[4];
                fresh[0] = i + r;
                holder[0] = fresh;
                checksum += fresh[0];
                stores++;
            }
            // Garbage, so a young pause actually fires and has to walk the
            // source set the loop above just rebuilt.
            int chunks = garbageKiB;
            for (int g = 0; g < chunks; g++) {
                byte[] dead = new byte[1024];
                dead[0] = (byte) g;
                checksum += dead[0] & 0xFF;
            }
        }

        long wallMs = (System.nanoTime() - start) / 1_000_000L;

        // Read the WHOLE retained set back at the very end, so nothing above is
        // a dead store and every filler object is genuinely live at the last
        // collection. This is also the detector: a source walk entered above an
        // object drops that object's edge, and the holder then points at a
        // young object whose region the pause freed.
        long alive = 0;
        for (int i = 0; i < n; i++) {
            if (i % HOLDER_STRIDE == 0) {
                Object[] holder = (Object[]) live[i];
                int[] young = (int[]) holder[0];
                alive += young[0];
            } else {
                alive += ((int[]) live[i])[0];
            }
        }

        System.out.println("G1BlockOffsetProbe"
                + " live=" + liveMiB + "MiB"
                + " elements=" + n
                + " holders=" + holderCount
                + " rounds=" + rounds
                + " stores=" + stores
                + " garbageKiB=" + garbageKiB
                + " wallMs=" + wallMs
                + " alive=" + alive
                + " checksum=" + checksum);
    }
}
