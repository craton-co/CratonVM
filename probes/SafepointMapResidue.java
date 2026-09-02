/**
 * A compiled frame that claims complete relocation coverage and still holds a
 * pre-move reference — in 25 lines, in about two minutes, with no H2.
 *
 * <p>`bug-h2-testrandommapops-small-heap-corruption-20260829.md` asked for
 * exactly this and named `String.substring(II)` at safepoint 41 as the witness,
 * reachable only through a 900 s `org.h2.test.store.TestRandomMapOps` run on a
 * quiet host. This reproduces the same defect through a smaller and steadier
 * witness.
 *
 * <h2>Running it</h2>
 *
 * <pre>
 *   javac -d /tmp/probe probes/SafepointMapResidue.java
 *   CRATONVM_DBG=remap-residue cratonvm --java-home $JDK25 --Xmx 192m \
 *       -cp /tmp/probe SafepointMapResidue
 * </pre>
 *
 * <p>Grep the output for `remap-frame` lines whose `stale_live` is not zero.
 * The witness is stable across runs:
 *
 * <pre>
 *   [remap-frame] method=java/lang/StringConcatHelper.doConcat:(...)
 *     sp_id=51 frame_size=1056 cov_complete=true live_hi=96
 *     mapped=[ 8=0x..ec40 16=0x..ed70 40=0x..0838 ] rewritten=1
 *     stale_words=18 stale_live=1 stale_dead=17
 *     [LIVE off=32 stale=0x20019013eb8->0x200102599b8]
 * </pre>
 *
 * <p>Read it as: slot 40 was named and rewritten to the object's new address;
 * word 32 holds the SAME object's OLD address, sits inside the live band
 * (`live_hi=96`), and the map never named it. The frame nevertheless reports
 * `cov_complete=true`, so relocation trusted it.
 *
 * <h2>What it is not</h2>
 *
 * <p><b>Not inlining.</b> Measured, because the shape invites the guess — the
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
 * <p>Turning inlining off does not remove them, so the missing word is not an
 * inline-scope local. It is the general case the page's own root-cause section
 * names: a live COPY of a reference in a frame word that is neither a
 * reference-parameter home nor a node's own slot is in neither source the map
 * is built from, and its absence does not clear `coverable`.
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
