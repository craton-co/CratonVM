/**
 * What did routing string concatenation through UTF-16 code units cost?
 *
 * `execute_string_concat` used to accumulate into a Rust `String` and finish
 * with `create_string_or_oom`, whose all-ASCII path is a `copy_nonoverlapping`.
 * It now accumulates `Vec<u16>` and finishes with
 * `create_string_from_units_or_oom` so an unpaired surrogate survives.
 *
 * That trade is not obviously free, and it is not obviously a loss either:
 *
 *   * REMOVED a transcode -- a `String` argument used to be decoded UTF-16 ->
 *     UTF-8 on the way in and re-encoded UTF-8 -> UTF-16 on the way out.
 *   * ADDED nothing to the all-ASCII memcpy, PROVIDED the Latin-1 units path
 *     bulk-writes. It did not before this change; it does now.
 *
 * Concatenation is the hottest allocation site in the VM (every log line, every
 * `toString`, every exception message), so "probably fine" is not good enough.
 * The record for this bug says in as many words: do not land without the
 * concat-heavy timings.
 *
 * Four workloads, because the paths differ:
 *   ascii    -- the overwhelmingly common case, Latin-1 in and Latin-1 out
 *   latin1   -- non-ASCII but still one byte per char (coder stays LATIN1)
 *   utf16    -- forces the two-byte path on both read and write
 *   mixed    -- primitives folded in beside strings (int/long/double args)
 *
 * Prints a checksum per workload: if the two arms disagree on it, the timings
 * are comparing different work and mean nothing.
 *
 * Run A-B-B-A interleaved against the pre-change binary. Serial blocks have
 * already reversed a conclusion once in this feature -- host load drifted
 * between them.
 */
public class StringConcatCostProbe {

    static final int ITERS = 200000;

    static long ascii(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            String s = "row-" + i + "-of-" + iters + "-tag";
            sum += s.length() + s.charAt(4);
        }
        return sum;
    }

    static long latin1(int iters) {
        String accent = "caf\u00E9";
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            String s = accent + i + "-\u00FC-" + accent;
            sum += s.length() + s.charAt(3);
        }
        return sum;
    }

    static long utf16(int iters) {
        String greek = "\u03A3\u039F\u03A3";
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            String s = greek + i + "-\u4E2D-" + greek;
            sum += s.length() + s.charAt(0);
        }
        return sum;
    }

    static long mixed(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            String s = "i=" + i + " l=" + (i * 1000000007L) + " d=" + (i * 0.5) + " b="
                    + (i % 2 == 0);
            sum += s.length();
        }
        return sum;
    }

    static long run(String name, int iters) {
        long t0 = System.currentTimeMillis();
        long sum;
        switch (name) {
            case "ascii":  sum = ascii(iters);  break;
            case "latin1": sum = latin1(iters); break;
            case "utf16":  sum = utf16(iters);  break;
            default:       sum = mixed(iters);  break;
        }
        long ms = System.currentTimeMillis() - t0;
        System.out.println(name + " ms=" + ms + " checksum=" + sum);
        return ms;
    }

    public static void main(String[] args) {
        String[] workloads = { "ascii", "latin1", "utf16", "mixed" };
        // Warm each workload before timing so the numbers are not dominated by
        // first-call class loading and tier-up.
        for (String w : workloads) {
            run(w + "-warmup", ITERS / 20);
        }
        for (int round = 0; round < 3; round++) {
            System.out.println("--- round " + round);
            for (String w : workloads) {
                run(w, ITERS);
            }
        }
        System.out.println("CONCAT-COST-PROBE-DONE");
    }
}
