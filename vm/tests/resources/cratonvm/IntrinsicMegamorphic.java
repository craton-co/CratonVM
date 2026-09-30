package cratonvm;

/**
 * Megamorphic call-site stress test for the interpreter intrinsic inline
 * cache.
 *
 * A monomorphic/polymorphic inline cache that caches an intrinsic resolution
 * must degrade GRACEFULLY when a single call site observes many distinct
 * receiver classes: it must not livelock re-resolving, and it must produce
 * correct results for every receiver. See roadmap risk "IC guard cost" and
 * test requirement "Megamorphic test".
 *
 * Two megamorphic sites are exercised:
 *   1. `cs.length()` where `cs` cycles through String plus several distinct
 *      CharSequence implementations -- some hit the String.length intrinsic,
 *      others must miss it and dispatch virtually.
 *   2. `o.hashCode()` where `o` cycles through plain Object, String, and
 *      several hashCode-overriding classes.
 *
 * Deterministic: prints a single fold of all observed values so the result
 * is identical with intrinsics on or off. The loop count is bounded; a hang
 * (IC livelock) is caught by the Rust-side subprocess timeout.
 *
 * Plain Java 8 syntax only.
 */
public class IntrinsicMegamorphic {

    // ---- distinct CharSequence implementations -------------------------
    static final class CsA implements CharSequence {
        public int length() { return 1; }
        public char charAt(int i) { return 'a'; }
        public CharSequence subSequence(int s, int e) { return this; }
    }
    static final class CsB implements CharSequence {
        public int length() { return 2; }
        public char charAt(int i) { return 'b'; }
        public CharSequence subSequence(int s, int e) { return this; }
    }
    static final class CsC implements CharSequence {
        public int length() { return 3; }
        public char charAt(int i) { return 'c'; }
        public CharSequence subSequence(int s, int e) { return this; }
    }
    static final class CsD implements CharSequence {
        public int length() { return 4; }
        public char charAt(int i) { return 'd'; }
        public CharSequence subSequence(int s, int e) { return this; }
    }

    // ---- distinct hashCode-overriding classes --------------------------
    static final class HcA { public int hashCode() { return 100; } }
    static final class HcB { public int hashCode() { return 200; } }
    static final class HcC { public int hashCode() { return 300; } }

    public static void main(String[] args) {
        // --- megamorphic site 1: cs.length() ---------------------------
        CharSequence[] seqs = {
                "x",                 // String -> intrinsic eligible (len 1)
                new CsA(),           // len 1
                new StringBuilder("yy"),  // StringBuilder -> CharSequence (len 2)
                new CsB(),           // len 2
                "zzz",               // String (len 3)
                new CsC(),           // len 3
                new CsD(),           // len 4
                new StringBuilder(), // len 0
        };
        long lenSum = 0;
        long lenIters = 0;
        for (int round = 0; round < 5000; round++) {
            for (int i = 0; i < seqs.length; i++) {
                lenSum += seqs[i].length();   // the megamorphic call site
                lenIters++;
            }
        }
        System.out.println("megamorphic.length.sum=" + lenSum);
        System.out.println("megamorphic.length.iters=" + lenIters);

        // --- megamorphic site 2: o.hashCode() --------------------------
        // Mix of: plain Object (identity hash -- non-deterministic, so we
        // only count consistency), String, and overriding classes.
        Object plain = new Object();
        int plainHc = plain.hashCode();
        String str = "fixed-string";
        int strHc = str.hashCode();
        Object[] objs = {
                new HcA(), new HcB(), new HcC(),
                plain, str,
                new HcA(), new HcC(),
        };
        long hcSum = 0;
        boolean plainConsistent = true;
        boolean strConsistent = true;
        for (int round = 0; round < 5000; round++) {
            for (int i = 0; i < objs.length; i++) {
                int h = objs[i].hashCode();   // the megamorphic call site
                if (objs[i] == plain && h != plainHc) plainConsistent = false;
                if (objs[i] == str && h != strHc) strConsistent = false;
                // Only fold deterministic receivers into the sum.
                if (objs[i] != plain) hcSum += h;
            }
        }
        System.out.println("megamorphic.hashCode.detsum=" + hcSum);
        System.out.println("megamorphic.hashCode.plainConsistent=" + plainConsistent);
        System.out.println("megamorphic.hashCode.strConsistent=" + strConsistent);
        // String.hashCode is spec-defined, so this value is portable.
        System.out.println("megamorphic.hashCode.strValue=" + strHc);

        boolean ok = (lenSum == (1L + 1 + 2 + 2 + 3 + 3 + 4 + 0) * 5000)
                && (lenIters == (long) seqs.length * 5000)
                && plainConsistent
                && strConsistent;
        System.out.println(ok ? "MEGAMORPHIC_OK" : "MEGAMORPHIC_FAIL");
    }
}
