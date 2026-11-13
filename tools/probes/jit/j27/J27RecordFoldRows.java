// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Objects;

/**
 * JIT j27 wave 5, lane RECORDS (RECORDS21-3, page
 * docs/feature-designs/jit-ir-optimizations.md):
 * facts for the record fold's reference shortcuts,
 * CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH (a String component's cached
 * hash before Objects.hashCode) and CRATONVM_JIT_IR_INDY_RECORD_FOLD_REF_IDENTITY
 * (a == b before Objects.equals), both default ON.
 *
 * Every stdout line must equal HotSpot 25 byte for byte, in both modes, on the
 * default arm, with each switch =0, and with CRATONVM_JIT_IR_INDY_RECORD_FOLD=0.
 * The expected lines are listed in the page. The record methods run WARM times
 * first so the optimizing tier compiles them before the facts are read; the
 * mismatch counters run over the same loops.
 *
 * Timing (stderr, "row <name> <ns/op>") only with -Dj27.time=1.
 */
public final class J27RecordFoldRows {
    static final int WARM = 200_000;

    record S(String a, String b) {}

    record N(int i, String s, Object o) {}

    enum Color { RED, GREEN }

    record E(Color c, String s) {}

    /** Counts its equals / hashCode calls; every instance equals every other. */
    static final class Counted {
        static int equalsCalls;
        static int hashCalls;

        @Override
        public boolean equals(Object o) {
            equalsCalls++;
            return o instanceof Counted;
        }

        @Override
        public int hashCode() {
            hashCalls++;
            return 7;
        }
    }

    record C(Counted c) {}

    static final class Boom {
        @Override
        public boolean equals(Object o) {
            throw new RuntimeException("boom-eq");
        }

        @Override
        public int hashCode() {
            return 1;
        }
    }

    record B(Boom b) {}

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    /** A String equal to s that is a fresh object with no cached hash. */
    static String fresh(String s) {
        return new String(s.toCharArray());
    }

    static int stringHashMismatches() {
        int bad = 0;
        for (int i = 0; i < WARM; i++) {
            String a = fresh("k" + (i & 1023));
            String b = (i % 7 == 0) ? null : fresh("v" + (i % 97));
            S r = new S(a, b);
            int first = r.hashCode();
            int want = 31 * Objects.hashCode(a) + Objects.hashCode(b);
            int second = r.hashCode();
            if (first != want || second != want) {
                bad++;
            }
        }
        return bad;
    }

    static int stringEqualsMismatches() {
        int bad = 0;
        for (int i = 0; i < WARM; i++) {
            String a = "k" + (i & 15);
            String b = (i % 5 == 0) ? null : "v" + (i % 3);
            S r = new S(a, b);
            S same = new S(a, b);
            S copy = new S(a == null ? null : fresh(a), b == null ? null : fresh(b));
            S other = new S(a, "w");
            boolean ok = r.equals(same) && r.equals(copy) && !r.equals(other) && r.equals(r)
                && !r.equals(null);
            if (!ok) {
                bad++;
            }
        }
        return bad;
    }

    static void countedFacts() {
        Counted shared = new Counted();
        C x = new C(shared);
        C y = new C(shared);
        C z = new C(new Counted());
        Counted.equalsCalls = 0;
        int equal = 0;
        for (int i = 0; i < WARM; i++) {
            equal += x.equals(y) ? 1 : 0;
        }
        fact("c-equals-shared", equal + " calls=" + Counted.equalsCalls);
        Counted.equalsCalls = 0;
        equal = 0;
        for (int i = 0; i < WARM; i++) {
            equal += x.equals(z) ? 1 : 0;
        }
        fact("c-equals-distinct", equal + " calls=" + Counted.equalsCalls);
        Counted.hashCalls = 0;
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += x.hashCode();
        }
        fact("c-hash", sum + " calls=" + Counted.hashCalls);
    }

    static String boomEquals(B a, B b) {
        try {
            return String.valueOf(a.equals(b));
        } catch (RuntimeException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static void boomFacts() {
        Boom shared = new Boom();
        B a = new B(shared);
        B same = new B(shared);
        B distinct = new B(new Boom());
        String s = "";
        String d = "";
        for (int i = 0; i < WARM; i++) {
            s = boomEquals(a, same);
            if (i % 1000 == 0) {
                d = boomEquals(a, distinct);
            }
        }
        fact("boom-shared-equals", s);
        fact("boom-distinct-equals", d);
    }

    static void literalFacts() {
        fact("s-hash-ab", new S("a", "b").hashCode());
        fact("s-hash-null-b", new S(null, "b").hashCode());
        fact("s-hash-empty", new S("", "").hashCode());
        fact("s-hash-nulls", new S(null, null).hashCode());
        fact("zero-hash-string", "f5a5a608".hashCode());
        fact("s-hash-zero-hash-string", new S("f5a5a608", "a").hashCode());
        fact("s-hash-fresh-zero-hash-string", new S(fresh("f5a5a608"), fresh("a")).hashCode());
        fact("n-hash-nulls", new N(1, null, null).hashCode());
        fact("n-hash-ab", new N(1, "a", "b").hashCode());
        fact("s-equals-literals", new S("a", "b").equals(new S("a", "b")));
        fact("s-equals-copies", new S("a", "b").equals(new S(fresh("a"), fresh("b"))));
        fact("s-equals-differ", new S("a", "b").equals(new S("a", "c")));
        fact("s-equals-null-null", new S(null, "b").equals(new S(null, "b")));
        fact("s-equals-null-value", new S(null, "b").equals(new S("a", "b")));
        fact("s-equals-value-null", new S("a", "b").equals(new S(null, "b")));
        fact("e-equals-same", new E(Color.RED, "x").equals(new E(Color.RED, "x")));
        fact("e-equals-other", new E(Color.RED, "x").equals(new E(Color.GREEN, "x")));
        E e = new E(Color.GREEN, "y");
        fact("e-hash-formula", e.hashCode() == 31 * Color.GREEN.hashCode() + "y".hashCode());
    }

    static void timeRows() {
        S hot = new S("alpha", "beta");
        S hot2 = new S("alpha", "beta");
        S copy = new S(fresh("alpha"), fresh("beta"));
        int reps = 2_000_000;
        long sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) {
            sink += hot.hashCode();
        }
        long t1 = System.nanoTime();
        for (int i = 0; i < reps; i++) {
            sink += hot.equals(hot2) ? 1 : 0;
        }
        long t2 = System.nanoTime();
        for (int i = 0; i < reps; i++) {
            sink += hot.equals(copy) ? 1 : 0;
        }
        long t3 = System.nanoTime();
        System.err.println("row hash-strings " + (t1 - t0) / reps);
        System.err.println("row equals-shared-strings " + (t2 - t1) / reps);
        System.err.println("row equals-copied-strings " + (t3 - t2) / reps);
        System.err.println("sink " + sink);
    }

    public static void main(String[] args) {
        fact("s-hash-mismatches", stringHashMismatches());
        fact("s-equals-mismatches", stringEqualsMismatches());
        countedFacts();
        boomFacts();
        literalFacts();
        if (Boolean.getBoolean("j27.time")) {
            for (int i = 0; i < 5; i++) {
                timeRows();
            }
        }
    }
}
