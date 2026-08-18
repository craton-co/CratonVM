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
 *   * **70 `int` locals declared first**, so the reference locals below land
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
 * ## Result, 2026-08-18: the cap is SAFE, and not for the stated reason
 *
 * `oop_reached=false oop_mask=0x0` — the mask really is entirely dark — while
 * locals 74/75/76 published `StackSlotRef(-600/-608/-616)`. What publishes them
 * is `classify_local_kinds`, which has no 64-slot cap; the oop mask is not the
 * reference authority above slot 63 and never was. `LowLocalOopProbe`, the same
 * shape under 64 locals, reports `oop_reached=true oop_mask=0x700000e`.
 *
 * Under `CRATONVM_MOVING_YOUNG=1` this probe also makes
 * `moving_young_safepoint_coverage_complete`'s own `num_locals > 64` refusal
 * visible: `CRATONVM_DBG=moving-young-coverage-dbg` prints
 * `[moving-young-coverage] incomplete: active frame map at rbp=…` 49 times here
 * and 0 times for the low twin, which is the collector diverting to the
 * non-moving sweep rather than relocating a frame it cannot fully describe.
 *
 *   javac -g -d . HighLocalOopProbe.java LowLocalOopProbe.java
 *   java -cp . HighLocalOopProbe 400000 4095                    # the control
 *   cratonvm --java-home <jdk> --Xmx 96m -cp . HighLocalOopProbe 400000 4095
 *   CRATONVM_DBG_EXCFRAME=1 cratonvm … HighLocalOopProbe 20000 65535  *     2>&1 | grep 'FRAME.*run'          # read oop_mask and the local slots
 *   CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG=moving-young-coverage-dbg cratonvm …  *     2>&1 | grep -c 'incomplete: active frame map'
 */
public final class HighLocalOopProbe {
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
        int l20 = 20;
        int l21 = 21;
        int l22 = 22;
        int l23 = 23;
        int l24 = 24;
        int l25 = 25;
        int l26 = 26;
        int l27 = 27;
        int l28 = 28;
        int l29 = 29;
        int l30 = 30;
        int l31 = 31;
        int l32 = 32;
        int l33 = 33;
        int l34 = 34;
        int l35 = 35;
        int l36 = 36;
        int l37 = 37;
        int l38 = 38;
        int l39 = 39;
        int l40 = 40;
        int l41 = 41;
        int l42 = 42;
        int l43 = 43;
        int l44 = 44;
        int l45 = 45;
        int l46 = 46;
        int l47 = 47;
        int l48 = 48;
        int l49 = 49;
        int l50 = 50;
        int l51 = 51;
        int l52 = 52;
        int l53 = 53;
        int l54 = 54;
        int l55 = 55;
        int l56 = 56;
        int l57 = 57;
        int l58 = 58;
        int l59 = 59;
        int l60 = 60;
        int l61 = 61;
        int l62 = 62;
        int l63 = 63;
        int l64 = 64;
        int l65 = 65;
        int l66 = 66;
        int l67 = 67;
        int l68 = 68;
        int l69 = 69;
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
        acc += l0 + l1 + l2 + l3 + l4 + l5 + l6 + l7 + l8 + l9 + l10 + l11 + l12 + l13 + l14 + l15 + l16 + l17 + l18 + l19 + l20 + l21 + l22 + l23 + l24 + l25 + l26 + l27 + l28 + l29 + l30 + l31 + l32 + l33 + l34 + l35 + l36 + l37 + l38 + l39 + l40 + l41 + l42 + l43 + l44 + l45 + l46 + l47 + l48 + l49 + l50 + l51 + l52 + l53 + l54 + l55 + l56 + l57 + l58 + l59 + l60 + l61 + l62 + l63 + l64 + l65 + l66 + l67 + l68 + l69;
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
        want += 0 + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12 + 13 + 14 + 15 + 16 + 17 + 18 + 19 + 20 + 21 + 22 + 23 + 24 + 25 + 26 + 27 + 28 + 29 + 30 + 31 + 32 + 33 + 34 + 35 + 36 + 37 + 38 + 39 + 40 + 41 + 42 + 43 + 44 + 45 + 46 + 47 + 48 + 49 + 50 + 51 + 52 + 53 + 54 + 55 + 56 + 57 + 58 + 59 + 60 + 61 + 62 + 63 + 64 + 65 + 66 + 67 + 68 + 69;
        check("main.acc", acc, want);
        System.out.println("sinkNonZero=" + (sink != 0 ? 1 : 0));
        System.out.println("failures=" + failures);
        System.out.println("OK");
        if (failures != 0) { System.exit(1); }
    }
}
