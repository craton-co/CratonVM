/**
 * Relocation under live JIT frames, exercised in 25 lines and about two minutes
 * with no H2 — the cheap way to see the stale-frame-word instrument work.
 *
 * <p>Written for
 * `bug-h2-testrandommapops-small-heap-corruption-20260829.md`, which named
 * `String.substring(II)` at safepoint 41 as a witness reachable only through a
 * 900 s `org.h2.test.store.TestRandomMapOps` run on a quiet host. This produces
 * the same SHAPE — a frame claiming complete coverage with a from-space word
 * still inside its live band — steadily and in seconds.
 *
 * <p><b>That shape is not a defect, and the CORRECTION section below says why.</b>
 * The word is dead spill residue, which is what the instrument's own oracle now
 * reports. This file carried the opposite claim for two days; it is retracted.
 *
 * <h2>Running it</h2>
 *
 * <pre>
 *   javac -d $OUT probes/SafepointMapResidue.java
 *   CRATONVM_DBG=remap-residue cratonvm --java-home $JDK25 --Xmx 192m \
 *       -cp $OUT SafepointMapResidue
 * </pre>
 *
 * <p>Read the `[remap-residue-summary]` line at exit. `local_oop` is the count
 * that names a MISSED ROOT; `frames` is the engagement counter that says the
 * instrument ran at all. Measured 2026-09-02, three runs, host at load 58-97:
 *
 * <pre>
 *   [remap-residue-summary] frames=28 frames_with_live_stale=1
 *     local_oop=0 local_not_oop=1 local_unreached=0
 *     frames_with_inline_scopes=0 inline_scopes=0
 *     inline_local_oop=0 inline_local_not_oop=0
 *     frames_with_stack_model=0 stack_not_oop=0
 *     duplicate_of_mapped=0 mapped_alias=0 outside_locals=0
 * </pre>
 *
 * <p>The flagged frame is stable across runs, and it is the one this probe was
 * written for:
 *
 * <pre>
 *   [remap-frame] method=java/lang/StringConcatHelper.doConcat:(...)
 *     sp_id=51 frame_size=1056 cov_complete=true live_hi=96
 *     local_mask=Some(19) num_locals=5
 *     mapped=[ 8=0x..ec40 16=0x..ed70 40=0x..98e8 ] rewritten=1
 *     inlined=["java/lang/String.&lt;init&gt;([BB)V"] scopes=[]
 *     stale_words=11 stale_live=1 stale_dead=10
 *     oracle=[local_oop=0 local_not_oop=1 ...]
 *     [LIVE off=32 k=3 local-not-oop region=java-local stale=0x..5988->0x..98e8]
 * </pre>
 *
 * <h2>CORRECTION 2026-09-02: this is NOT a missed root, and never was</h2>
 *
 * <p>An earlier version of this file read the line above as a defect -- "word 32
 * sits inside the live band and the map never named it, and the frame still
 * claims complete coverage". That reading is <b>withdrawn</b>. `live_frame_hi`
 * is the operand-spill CURSOR: a watermark, not a liveness bound. The cursor
 * reclaims by moving, not by clearing, so a word an earlier expression
 * abandoned below it still counts as "live" to that test. `stale_live` is
 * therefore an upper bound on missed roots and cannot be read as a verdict.
 *
 * <p>Offset 32 is java local 3 (`local_offset(k) == 8*(k+1)`), and `javap -c` on
 * `doConcat` shows `25: istore_3` -- local 3 holds an <b>int</b> at every
 * safepoint where the mask reads `Some(19)` (= locals 0, 1 and 4, i.e. offsets
 * 8, 16 and 40, exactly the three the map named). The stale pointer at 32 is
 * dead storage from before that `istore`.
 *
 * <p>The report now says this itself: `local-not-oop` is the compiler's own
 * forward "must be oop" dataflow answering for that slot, carried on the map as
 * `OopMapEntry::local_oop_mask`. Only `LOCAL-OOP-UNMAPPED` names a missed root,
 * and this probe produces none.
 *
 * <p><b>What the probe is still good for.</b> It is a cheap, deterministic
 * exerciser of relocation under live JIT frames and the fastest way to see the
 * residue instrument work end to end. What it does NOT do is exhibit a defect.
 *
 * <p><b>What it does not yet reach.</b> `frames_with_inline_scopes=0` and
 * `frames_with_stack_model=0`: no reported frame has a safepoint INSIDE a
 * splice, and none carries a frame-resident operand entry the stack model calls
 * a non-reference, so two of the oracle's four halves are armed but unexercised
 * here and their zeros must not be read as evidence. The frames do carry
 * `inlined=[...]`, which is the method list, not a live scope at that bci.
 * `org.h2.test.store.TestRandomMapOps` does exercise the rest -- see the
 * 2026-09-02 addendum on the page above.
 *
 * <h2>What it is not</h2>
 *
 * <p><b>Not inlining.</b> Measured before the oracle existed, when the right
 * column was still believed to count missed roots. It counts FLAGGED FRAMES,
 * and only the default arm has since been re-measured with the oracle (all
 * dead storage); the ablation is kept because it still rules inline scopes out
 * as the SOURCE of the residue. The shape invites the guess — the
 * frame carries an inlined `String.&lt;init&gt;([BB)V` and `fully_oop_covered`
 * has an `inline_sites.is_empty()` term, so "the inlined callee's locals are
 * unnamed" reads as the obvious answer. Ablated on this probe:
 *
 * <pre>
 *   arm                             frames  frames with a LIVE stale word
 *   default                             29                             1
 *   CRATONVM_JIT_INLINE_CALLS=0         24                             3
 *   CRATONVM_JIT_INLINE=0               25                             3
 * </pre>
 *
 * <p>Turning inlining off does not remove them, so the residue is not an
 * inline-scope local. In the DEFAULT arm the oracle now says what they are
 * instead: dead spill below the watermark. The two ablation arms have not been
 * re-run under the oracle, so nothing is claimed about how their words classify.
 *
 * <p><b>Not the relocation gate.</b> The run above is the shipped default, with
 * `CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE` ON. The gate declines relocation
 * for maps the compiler has already JUDGED short; these maps are not judged
 * short — `causes(...)` reports zero on this method — so the gate never sees
 * them.
 */
public class SafepointMapResidue {

    static Object sink;

    /** Allocates, so the loop below reaches a collection. */
    static String pad() {
        return new String(new char[32]);
    }

    /**
     * Two live references held across two GC points. `concat` reaches
     * `StringConcatHelper.doConcat`, whose `new String(byte[], byte)` leaves a
     * duplicated operand-stack reference — the word the map misses.
     */
    static int hot(String a, String b) {
        String x = a.concat("-");
        String y = b.concat("+");
        sink = y;
        return x.length() + y.length() + a.length() + b.length();
    }

    public static void main(String[] args) {
        String a = "alpha";
        String b = "beta";
        long n = 0;
        java.util.ArrayList<Object> keep = new java.util.ArrayList<>();
        for (int i = 0; i < 3_000_000; i++) {
            n += hot(a, b);
            if ((i & 127) == 0) {
                keep.add(pad());
                if (keep.size() > 3000) {
                    keep.subList(0, 1500).clear();
                }
            }
        }
        System.out.println("@@ done n=" + n);
    }
}
