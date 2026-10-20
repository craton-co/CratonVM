// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gci i4/wr: {@code String.intern()} of a String that holds an UNPAIRED
 * surrogate, which this VM keeps in a pool of its own (keyed by UTF-16 units,
 * {@code lang_string::intern_unrepresentable}) because Rust text cannot hold
 * it. Page {@code docs/internal/gc/gcg-g2w-string-table-review-findings-RETIRED-UNFIXED-20261018.md}
 * item 2: that pool held STRONG handles, so such a String was immortal where
 * HotSpot's weak StringTable lets it die.
 *
 * Lines (stdout, deterministic on HotSpot 25):
 *  identity      two equal lone-surrogate Strings intern to one object, and the
 *                first receiver is that object (JLS: the receiver is pooled);
 *  literal       a lone-surrogate LITERAL is the canonical instance for its
 *                text, and survives System.gc() while its class lives;
 *  cleared       an interned lone-surrogate String nothing references is
 *                collected (its WeakReference clears within 5 System.gc());
 *  reinterned    after that, a fresh equal String interns to ITSELF (the dead
 *                pool row was replaced), and stays canonical.
 * Diagnostics (gc rounds needed) go to stderr.
 */
public class GciWr4SurrogateInternProbe {
    static final String LIT = "\uD800lit";

    static char[] units(String tail) {
        char[] c = new char[tail.length() + 1];
        c[0] = '\uD800';
        tail.getChars(0, tail.length(), c, 1);
        return c;
    }

    /** Intern in a frame of its own; only the WeakReference leaves it. */
    static WeakReference<String> internAndDrop(String tail) {
        String s = new String(units(tail));
        String i = s.intern();
        if (i != s) {
            System.out.println("internAndDrop: receiver not pooled");
        }
        return new WeakReference<>(i);
    }

    static int gcUntilCleared(WeakReference<?> w) {
        for (int round = 1; round <= 5; round++) {
            System.gc();
            if (w.get() == null) {
                return round;
            }
            try {
                Thread.sleep(20);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        return -1;
    }

    public static void main(String[] args) {
        String a = new String(units("same"));
        String b = new String(units("same"));
        String ia = a.intern();
        String ib = b.intern();
        System.out.println("identity: same=" + (ia == ib) + " receiver=" + (ia == a));

        String runtime = new String(units("lit")).intern();
        WeakReference<String> lw = new WeakReference<>(LIT);
        System.gc();
        System.out.println("literal: canonical=" + (runtime == LIT) + " kept=" + (lw.get() == LIT));

        WeakReference<String> w = internAndDrop("dropped");
        int rounds = gcUntilCleared(w);
        System.err.println("[gci-wr4] cleared after rounds=" + rounds);
        System.out.println("cleared: " + (rounds > 0));

        String fresh = new String(units("dropped"));
        String again = fresh.intern();
        String twice = new String(units("dropped")).intern();
        System.out.println("reinterned: receiver=" + (again == fresh) + " canonical=" + (twice == again));
    }
}
