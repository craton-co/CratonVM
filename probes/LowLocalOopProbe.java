/**
 * Does a REFERENCE local above slot 63 survive an OSR'd, GC-ing, deopt-taking
 * loop in a method with more than 64 locals?
 *
 * `compute_local_oop_masks` returns EMPTY vectors for `max_locals > 64`, so the
 * deopt snapshot's `is_oop = i < 64 && (oop_mask & …)` reads FALSE for every
 * local in such a method — not just the ones above 63. The comment at that site
 * says this is "sound only because `can_deopt_resume` (later) gates such methods
 * off", and `can_deopt_resume` is
 * `!deopt_points.is_empty() && !has_elided_monitor`, which says nothing about the
 * local count. This probe is the witness for whether something else holds.
 *
 * Shape, all in one once-called method so OSR is the only compile door:
 *
 *   * **20 `int` locals declared first (the CONTROL: total locals stay under 64)**, so the reference locals below land
 *     above slot 63 (`javap` confirms the slot numbers — check them, do not
 *     assume javac's allocation);
 *   * **`tag` and `arr`, reference locals, READ on every iteration** — a
 *     reference published as a non-oop `StackSlot` resumes as a `Value::Int`,
 *     and an `aload` of it then reads null, so the read is what turns a silent
 *     mis-typing into a `NullPointerException` or a wrong length;
 *   * **the receiver class changes halfway**, so the `invokeinterface` site's
 *     speculative guard takes a real `ReceiverTypeChanged` deopt exit out of the
 *     OSR'd body with those references live;
 *   * **`System.gc()` every 64Ki iterations**, so a compacting collector has to
 *     find and (if it relocates) rewrite those references while the OSR'd frame
 *     holds them. If they are invisible to the root scan, this is where the
 *     address goes stale.
 *
 * Every printed number has a closed form, so a wrong answer is a mismatch and
 * not a judgement call.
 *
 * This is the CONTROL for `HighLocalOopProbe`: byte-for-byte the same shape with
 * 50 fewer `int` locals, so the total stays under 64 and the reference locals
 * land at slots 24/25/26. It reports `oop_reached=true oop_mask=0x700000e` and
 * zero `moving-young-coverage incomplete` lines where its twin reports
 * `oop_mask=0x0` and 49 of them. Without it, "the mask was empty" is an
 * observation about one probe rather than about the local count.
 */
public final class LowLocalOopProbe {
    interface Op { int apply(int x); }
    static final class A implements Op { public int apply(int x) { return x + 1; } }
    static final class B implements Op { public int apply(int x) { return x + 2; } }

    static long sink;
    static int failures;
    /** How often the loop forces a collection; overridden from argv[1]. */
    static int GC_MASK = 0xFFFF;

    static void check(String what, long got, long want) {
        boolean ok = got == want;
        if (!ok) { failures++; }
        System.out.println((ok ? "PASS " : "FAIL ") + what + " got=" + got + " want=" + want);
    }

    /** Called ONCE. > 64 locals; the reference locals sit above slot 63. */
    static long run(int n, Op a, Op b, String expectTag) {
        int l0 = 0;
        int l1 = 1;
        int l2 = 2;
        int l3 = 3;
        int l4 = 4;
        int l5 = 5;
        int l6 = 6;
        int l7 = 7;
        int l8 = 8;
        int l9 = 9;
        int l10 = 10;
        int l11 = 11;
        int l12 = 12;
        int l13 = 13;
        int l14 = 14;
        int l15 = 15;
        int l16 = 16;
        int l17 = 17;
        int l18 = 18;
        int l19 = 19;
        String tag = expectTag;
        int[] arr = new int[4];
        Object obj = new Object();
        long acc = 0;
        int nulls = 0;
        int wrongLen = 0;
        int wrongIdentity = 0;
        for (int i = 0; i < n; i++) {
            Op o = (i < (n >> 1)) ? a : b;
            acc += o.apply(i);
            // Read the high reference locals EVERY iteration.
            if (tag == null) { nulls++; }
            else if (tag.length() != 8) { wrongLen++; }
            if (arr == null) { nulls++; }
            else { arr[i & 3] = i; }
            if (obj == null) { nulls++; }
            // Allocation churn, so a compacting collector has real work to do
            // and the frame-resident high-slot references are held across a
            // cycle that is actually relocating rather than a no-op sweep.
            byte[] garbage = new byte[64];
            garbage[0] = (byte) i;
            sink += garbage[0];
            if ((i & GC_MASK) == 0) { System.gc(); }
        }
        if (tag != expectTag) { wrongIdentity++; }
        acc += l0 + l1 + l2 + l3 + l4 + l5 + l6 + l7 + l8 + l9 + l10 + l11 + l12 + l13 + l14 + l15 + l16 + l17 + l18 + l19;
        check("run.nulls", nulls, 0);
        check("run.wrongLen", wrongLen, 0);
        check("run.wrongIdentity", wrongIdentity, 0);
        check("run.tagLength", tag.length(), 8);
        check("run.tagContent", tag.equals("tag-abcd") ? 1 : 0, 1);
        check("run.arrLast", arr[(n - 1) & 3], n - 1);
        sink += acc;
        return acc;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400000;
        if (args.length > 1) { GC_MASK = Integer.parseInt(args[1]); }
        // 8 chars, built at runtime so it is not an interned `ldc` constant the
        // compiler could fold — a folded constant would never test the slot.
        String tag = new StringBuilder("tag-").append("abcd").toString();
        long acc = run(n, new A(), new B(), tag);
        // sum(o.apply(i)) = sum(i+1) for i<n/2 plus sum(i+2) for i>=n/2
        long half = n >> 1;
        long want = 0;
        for (long i = 0; i < half; i++) { want += i + 1; }
        for (long i = half; i < n; i++) { want += i + 2; }
        want += 0 + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12 + 13 + 14 + 15 + 16 + 17 + 18 + 19;
        check("main.acc", acc, want);
        System.out.println("sinkNonZero=" + (sink != 0 ? 1 : 0));
        System.out.println("failures=" + failures);
        System.out.println("OK");
        if (failures != 0) { System.exit(1); }
    }
}
