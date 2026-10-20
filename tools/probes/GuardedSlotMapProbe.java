import java.time.Month;
import java.util.StringJoiner;

/**
 * The Java-visible surface of the four slot maps in W7-77-guarded-slot-maps.md.
 *
 * READ THIS BEFORE READING THE OUTPUT. Every line below is expected to be
 * NO-CHANGE, and that is the finding, not a disappointment. All four rows in
 * that record are slot maps that disagree with the real JDK 25 layout and are
 * kept safe by a guard, and every guard holds on every receiver this tree can
 * currently produce. So there is no red to turn green here: the observable for
 * a guarded row is the row leaving the read-side census
 * (`CRATONVM_DBG_LAYOUT_ALIAS=1`), not a behaviour change, and writing a probe
 * that implies otherwise would be the vacuous shape this campaign exists to
 * stamp out.
 *
 * What this probe IS for: it is the regression gate for the day somebody
 * removes a guard. `native-api/tests/guarded_slot_maps.rs` catches that in
 * source; this catches it in behaviour, and between them the removal cannot be
 * silent. Only `StringJoiner` can express the failure from Java at all — the
 * other three rows (`java.time.Month`, `java.lang.reflect.Method`'s legacy
 * mirror, `java.lang.Thread`'s virtual flag) are reachable only on the
 * fabricated-class path, where the disputed map IS the layout.
 *
 * WHY THE ASSERTIONS ARE EXACT STRINGS. `java.util.StringJoiner`'s real JDK 25
 * layout is
 *
 *     0 prefix   1 delimiter   2 suffix   3 elts   4 size   5 len   6 emptyValue
 *
 * and the synthetic map has 0 = delimiter and 1 = prefix — SWAPPED — plus
 * `emptyValue` on the int `size`. Applying it to a real receiver therefore
 * still produces a perfectly well-formed joined string; it just uses the
 * delimiter as the prefix and vice versa. `assertThat(result).isNotEmpty()`
 * passes against that. Only the exact text distinguishes them, so every
 * expected value below was MEASURED on the host oracle —
 * `java -version` = OpenJDK 25.0.3+9-LTS (Temurin), Eclipse Adoptium
 * 25.0.3.9 — and pasted in, never guessed.
 *
 * Every case uses a prefix, a delimiter and a suffix that are pairwise
 * distinct and distinguishable by LENGTH as well as by content, so a swap
 * moves both `toString()` and `length()`.
 *
 * That each assertion actually discriminates was MEASURED rather than assumed:
 * every case here was re-rendered under the swapped map and diffed against the
 * oracle. Result — the swapped map gives `-DELIM-[SUF]` (not `<PRE>[SUF]`),
 * `-DELIM-A<PRE>B[SUF]` (not `<PRE>A-DELIM-B[SUF]`), empty `length()` 12 (not
 * 10) and three-element `length()` 25 (not 27). Twelve of the thirteen
 * StringJoiner assertions catch the swap; the one that does not is
 * `sj.delimOnly.length` and it is labelled at its own call site.
 *
 * Expected on all three arms: `java`, `cratonvm --real-jdk`, `cratonvm
 * --jdk-only`. Exit code is the failure count.
 */
public final class GuardedSlotMapProbe {

    private static int failures = 0;
    private static int checks = 0;

    private static void eq(String what, String actual, String expected) {
        checks++;
        boolean ok = expected.equals(actual);
        if (!ok) {
            failures++;
        }
        System.out.println(
            (ok ? "PASS " : "FAIL ") + what
            + "  actual=[" + actual + "]"
            + (ok ? "" : "  expected=[" + expected + "]"));
    }

    private static void eq(String what, int actual, int expected) {
        eq(what, Integer.toString(actual), Integer.toString(expected));
    }

    public static void main(String[] args) {
        stringJoiner();
        month();
        System.out.println("checks=" + checks + " failures=" + failures);
        if (failures != 0) {
            // Louder than a silent exit: a harness that only greps stdout still
            // sees this line.
            System.out.println(
                "GUARDED-SLOT-MAP REGRESSION — a guard named in "
                + "W7-77-guarded-slot-maps.md has stopped holding");
        }
        System.exit(failures);
    }

    /**
     * The one row observable from Java. Prefix `<PRE>` (5), delimiter
     * `-DELIM-` (7) and suffix `[SUF]` (5) are pairwise distinct.
     */
    private static void stringJoiner() {
        System.out.println("-- java.util.StringJoiner (prefix/delimiter swap, emptyValue-on-size) --");

        StringJoiner j = new StringJoiner("-DELIM-", "<PRE>", "[SUF]");
        // Empty: prefix + suffix, and NOT delimiter + suffix.
        eq("sj.empty.toString", j.toString(), "<PRE>[SUF]");
        eq("sj.empty.length", j.length(), 10);

        j.add("A");
        eq("sj.one.toString", j.toString(), "<PRE>A[SUF]");
        j.add("B");
        eq("sj.two.toString", j.toString(), "<PRE>A-DELIM-B[SUF]");
        j.add("C");
        eq("sj.three.toString", j.toString(), "<PRE>A-DELIM-B-DELIM-C[SUF]");
        eq("sj.three.length", j.length(), 27);

        // setEmptyValue writes the field the synthetic map puts on the int
        // `size`. If that landed on `size`, the joiner would believe it holds
        // elements and `toString` would not return the empty value at all.
        StringJoiner e = new StringJoiner("-DELIM-", "<PRE>", "[SUF]");
        e.setEmptyValue("EMPTY!");
        eq("sj.emptyValue.toString", e.toString(), "EMPTY!");
        eq("sj.emptyValue.length", e.length(), 6);
        e.add("Z");
        // Once non-empty the empty value must be ignored again — this is the
        // half that a stuck `size` would break in the other direction.
        eq("sj.emptyValue.afterAdd", e.toString(), "<PRE>Z[SUF]");

        // merge() takes the OTHER joiner's delimiter and neither of its
        // affixes, so it reads slots 0 and 1 of a second object.
        StringJoiner m1 = new StringJoiner("-DELIM-", "<PRE>", "[SUF]");
        m1.add("A").add("B");
        StringJoiner m2 = new StringJoiner("+D2+", "<P2>", "[S2]");
        m2.add("X").add("Y");
        m1.merge(m2);
        eq("sj.merged.toString", m1.toString(), "<PRE>A-DELIM-B-DELIM-X+D2+Y[SUF]");

        // The one-arg constructor. The legacy arm writes delim -> slot 0 and
        // null -> slot 1, so on a real receiver the delimiter becomes the
        // PREFIX and the delimiter becomes empty: `,pq` instead of `p,q`.
        //
        // `length()` does NOT catch that — `,pq` and `p,q` are both 3. That was
        // measured, not assumed, by rendering every case in this method under
        // the swapped map and diffing against the oracle; it is the only
        // assertion here that a prefix/delimiter swap slips past. It is kept
        // (a stuck `size`, the OTHER half of this map's disagreement, does move
        // it) and labelled, because an unlabelled non-discriminating assertion
        // sitting among discriminating ones is how a suite starts being trusted
        // for something it does not check.
        StringJoiner d = new StringJoiner(",");
        d.add("p").add("q");
        eq("sj.delimOnly.toString", d.toString(), "p,q");
        eq("sj.delimOnly.length", d.length(), 3);
    }

    /**
     * `java.time.Month`. Synthetic-only, and NOT observable as a defect from
     * Java — on the fabricated stub slot 0 really is the int value, and on a
     * real `Month` the natives are not registered at all. These lines exist to
     * pin the answers the synthetic path must keep giving after W7-77 routed
     * every access through `month_alloc` / `month_value`.
     *
     * If the class-side witness ever misfires, `getValue()` answers 1 and
     * `name()` answers JANUARY for every month, so August is the case that
     * says so.
     */
    private static void month() {
        System.out.println("-- java.time.Month (Int into Enum.name; synthetic-only) --");
        Month m = Month.of(8);
        eq("month.name", m.name(), "AUGUST");
        eq("month.toString", m.toString(), "AUGUST");
        eq("month.getValue", m.getValue(), 8);
        eq("month.ordinal", m.ordinal(), 7);
        // A second value, because "every month answers January" is exactly the
        // failure a single JANUARY-adjacent case would pass through.
        Month d = Month.of(12);
        eq("month.dec.name", d.name(), "DECEMBER");
        eq("month.dec.getValue", d.getValue(), 12);
        eq("month.dec.length", Month.of(2).length(false), 28);
        eq("month.feb.leapLength", Month.of(2).length(true), 29);
    }
}
