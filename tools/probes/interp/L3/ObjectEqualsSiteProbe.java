// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 3, lane L3: `Object.equals(Object)` call sites are
// cached again (they were excluded from every invoke cache since 2026-07-15).
//
// Correctness: one method calls `x.equals(y)` and `y.equals(x)` -- two bytecode
// offsets sharing ONE `Object.equals` Methodref -- with receivers of classes
// whose `equals` are deliberately asymmetric, alternating receiver classes at
// the same offsets (the Brave `WeakKey` / `TraceContext` shape), plus a
// HashMap keyed by both classes. Every line of stdout must match HotSpot 25
// exactly; with the exclusion removed a receiver-unguarded entry would show up
// here as a wrong count.
//
// Expected (HotSpot 25):
//   forward=100000 backward=0 self=200000
//   map-hits=200000 map-size=4
//   mixed=100000
//
// Performance: the timed loop (stderr) is `Objects.equals` on String keys --
// every call was a slow-path invoke before; compare `--nojit` timings against
// the previous build, and `CRATONVM_INVOKE_CACHE_STATS=1` should now report
// the equals sites as hits.
import java.util.HashMap;
import java.util.Map;
import java.util.Objects;

public class ObjectEqualsSiteProbe {
    /** Equal to any `Loose` or `Strict`. */
    static final class Loose {
        final int id;

        Loose(int id) {
            this.id = id;
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof Loose || o instanceof Strict;
        }

        @Override
        public int hashCode() {
            return id;
        }
    }

    /** Equal only to a `Strict` with the same id. */
    static final class Strict {
        final int id;

        Strict(int id) {
            this.id = id;
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof Strict && ((Strict) o).id == id;
        }

        @Override
        public int hashCode() {
            return id;
        }
    }

    static int forward, backward, self;

    /** Two offsets, one Methodref: `x.equals(y)` then `y.equals(x)`. */
    static void both(Object x, Object y) {
        if (x.equals(y)) forward++;
        if (y.equals(x)) backward++;
        if (x.equals(x)) self++;
        if (y.equals(y)) self++;
    }

    static boolean site(Object a, Object b) {
        return a.equals(b);
    }

    public static void main(String[] args) {
        Loose loose = new Loose(1);
        Strict strict = new Strict(2);
        for (int i = 0; i < 100_000; i++) {
            both(loose, strict);
        }
        System.out.println("forward=" + forward + " backward=" + backward + " self=" + self);

        Map<Object, Integer> map = new HashMap<>();
        map.put(new Strict(1), 1);
        map.put(new Strict(2), 2);
        map.put(new Loose(3), 3);
        map.put(new Loose(4), 4);
        int hits = 0;
        for (int i = 0; i < 50_000; i++) {
            if (map.get(new Strict(1)) != null) hits++;
            if (map.get(new Strict(2)) != null) hits++;
            if (map.get(new Loose(3)) != null) hits++;
            if (map.get(new Loose(4)) != null) hits++;
        }
        System.out.println("map-hits=" + hits + " map-size=" + map.size());

        // One site, receivers rotating through three classes.
        Object[] receivers = {loose, strict, "s"};
        Object[] arguments = {strict, new Strict(3), "s"};
        int mixed = 0;
        for (int i = 0; i < 150_000; i++) {
            if (site(receivers[i % 3], arguments[i % 3])) mixed++;
        }
        System.out.println("mixed=" + mixed);

        String[] keys = new String[64];
        for (int i = 0; i < keys.length; i++) keys[i] = "key-" + i;
        long t0 = System.nanoTime();
        int eq = 0;
        for (int r = 0; r < 20; r++) {
            for (int i = 0; i < 10_000; i++) {
                if (Objects.equals(keys[i & 63], keys[(i * 7) & 63])) eq++;
            }
        }
        long ns = System.nanoTime() - t0;
        System.err.println("Objects.equals x200000: " + (ns / 1_000_000) + " ms (eq=" + eq + ")");
    }
}
