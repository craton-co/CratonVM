// JIT round j27 wave 6, lane T1EA (book docs/feature-designs/jit-ir-optimizations.md):
// deopt-rematerialisation facts for the EA21 folds, with the transfer placed MID-LOOP.
//
// Why a new probe: every loop row of R21EaRows / T1JiteaRows tested `rare && j == K`. With `rare`
// false on every warm call the never-taken edge is the one out of `rare`'s own test, so the
// transfer to the interpreter happened at iteration 0 -- before any box web or loop-carried field
// held the value the row compared -- and the comparison ran on interpreter-built boxes: the rows
// could not fail on a wrong box recipe. (Those two probes are fixed in the same wave by testing
// `j == K && rare`.) Every loop row here tests `j == K && rare`: `j == K` is taken on every warm
// call, so the never-taken edge is `rare`'s, at iteration K, with the elided objects holding the
// values below.
//
// Each `row` method is called WARM times with rare == false, then once with rare == true; the rare
// arm also touches a class nothing has initialised (LateN), which forces a deopt in a tier that
// kept the branch. One `fact` line per row; `time` lines only with -Dj27.time=true.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/j27/J27T1EaRows.java):
//   P=tools/probes/out
//   java -Xmx512m -cp $P J27T1EaRows                                   # HotSpot 25
//   cratonvm -Xmx512m -cp $P J27T1EaRows                               # jdk-only
//   cratonvm --compatible -Xmx512m -cp $P J27T1EaRows                  # compatible
//   CRATONVM_DBG_SCALAR_DEOPT=1 cratonvm -cp $P J27T1EaRows 2>&1 | grep -c MATERIALIZE   # > 0
// Arms (every `fact` line identical in each): CRATONVM_JIT_IR_BOX_PHI_WEB_FOLD=0,
//   CRATONVM_JIT_DEOPT_BOX_NO_JAVA=0, CRATONVM_JIT_EA_REF_COMPARE_FOLD=0,
//   CRATONVM_JIT_EA_ROUND_NULL_COMPARE_FOLD=0, CRATONVM_JIT_EA_FRESH_TYPECHECK_FOLD=1, --nojit.
//
// Expected HotSpot 25 output (JLS 5.1.7 with the default cache: Integer and Long -128..127; a
// valueOf outside it allocates a fresh box, so two such boxes are distinct):
//   fact box-edge-127 111        (s=-127 cached, l=127 cached, values right)
//   fact box-edge-128 101        (s=-128 cached, l=128 NOT cached)
//   fact box-edge-129 100        (s=-129 NOT cached, l=129 NOT cached)
//   fact box-edge-warm 0
//   fact box-two-webs 1101       (a==b at 6: same cached box; c==d at 1006: two boxes; equals true)
//   fact box-two-webs-warm 0
//   fact box-and-holder 1111     (never-stored ref fields null, box cached, loop-carried int right)
//   fact box-and-holder-warm 0
//   fact ref-cmp-deopt 110       (two fresh objects: distinct, same field values)
//   fact ref-cmp-deopt-warm 0
//   fact null-phi 110
//   fact null-phi-warm 400000
//   fact optional-deopt 11
//   fact optional-deopt-warm 0
//   fact late-hits 8

import java.util.Optional;

public class J27T1EaRows {
    static final int WARM = 40_000;

    static final class P {
        int x;

        P() {}
    }

    static final class Node {
        int v;
        Node next;
        Object tag;

        Node() {}
    }

    static int lateHits;

    static final class Late1 { static { lateHits++; } static void touch() {} }
    static final class Late2 { static { lateHits++; } static void touch() {} }
    static final class Late3 { static { lateHits++; } static void touch() {} }
    static final class Late4 { static { lateHits++; } static void touch() {} }
    static final class Late5 { static { lateHits++; } static void touch() {} }
    static final class Late6 { static { lateHits++; } static void touch() {} }
    static final class Late7 { static { lateHits++; } static void touch() {} }
    static final class Late8 { static { lateHits++; } static void touch() {} }

    // s falls by one and l rises by one per iteration: at j, s == -121 - j and l == 121 + j.
    // The three rows put the transfer at the cache edge. Separate methods so each compiles alone.
    static int boxEdgeCheck(Integer s, Long l, int j) {
        int sv = -121 - j;
        long lv = 121L + j;
        int r = (s == Integer.valueOf(sv)) ? 1 : 0;
        r += (l == Long.valueOf(lv)) ? 10 : 0;
        r += (s.intValue() == sv && l.longValue() == lv) ? 100 : 0;
        return r;
    }

    static int boxEdge127(int i, boolean rare) {
        Integer s = -120;
        Long l = 120L;
        int r = 0;
        int n = 12 + (i & 3);
        for (int j = 0; j < n; j++) {
            s -= 1;
            l += 1;
            if (j == 6 && rare) {
                Late1.touch();
                r += boxEdgeCheck(s, l, j);
            }
        }
        return r + s + n + 120 + (int) (l - 120 - n);
    }

    static int boxEdge128(int i, boolean rare) {
        Integer s = -120;
        Long l = 120L;
        int r = 0;
        int n = 12 + (i & 3);
        for (int j = 0; j < n; j++) {
            s -= 1;
            l += 1;
            if (j == 7 && rare) {
                Late2.touch();
                r += boxEdgeCheck(s, l, j);
            }
        }
        return r + s + n + 120 + (int) (l - 120 - n);
    }

    static int boxEdge129(int i, boolean rare) {
        Integer s = -120;
        Long l = 120L;
        int r = 0;
        int n = 12 + (i & 3);
        for (int j = 0; j < n; j++) {
            s -= 1;
            l += 1;
            if (j == 8 && rare) {
                Late3.touch();
                r += boxEdgeCheck(s, l, j);
            }
        }
        return r + s + n + 120 + (int) (l - 120 - n);
    }

    // Two box webs per width, named by one snapshot: the two cached ones must come back as ONE
    // object, the two uncached ones as two.
    static int boxTwoWebs(int i, boolean rare) {
        Integer a = 0;
        Integer b = 0;
        Integer c = 1000;
        Integer d = 1000;
        int r = 0;
        int n = 8 + (i & 3);
        for (int j = 0; j < n; j++) {
            a += j;
            b += j;
            c += j;
            d += j;
            if (j == 3 && rare) {
                Late4.touch();
                r += (a == b) ? 1 : 0;
                r += (c == d) ? 10 : 0;
                r += (a == Integer.valueOf(6)) ? 100 : 0;
                r += c.equals(d) ? 1000 : 0;
            }
        }
        return r + a - b + c - d;
    }

    // A box web and a scalar-replaced holder with never-stored reference fields in one frame
    // (the auto-box recipe and the j27 wave 5 zero-default rule in one materialisation).
    static int boxAndHolder(int i, boolean rare) {
        Node h = new Node();
        h.v = i;
        Integer s = 0;
        int r = 0;
        int n = 8 + (i & 3);
        for (int j = 0; j < n; j++) {
            s += j;
            h.v += j;
            if (j == 3 && rare) {
                Late5.touch();
                r += (h.next == null) ? 1 : 0;
                r += (h.tag == null) ? 10 : 0;
                r += (s == Integer.valueOf(6)) ? 100 : 0;
                r += (h.v == i + 6) ? 1000 : 0;
            }
        }
        int t = n * (n - 1) / 2;
        return r + s - t + h.v - i - t;
    }

    // EA21-1: the compare folds to false; after the transfer the two objects are two objects.
    static int refCmpDeopt(int i, boolean rare) {
        P a = new P();
        a.x = i;
        P b = new P();
        b.x = i;
        int r = (a == b) ? 1 : 0;
        if (rare) {
            Late6.touch();
            r += (a != b) ? 10 : 0;
            r += (a.x == b.x) ? 100 : 0;
        }
        return r;
    }

    // The per-round null-compare fold (default ON since j27): a φ of two allocations.
    static int nullPhi(int i, boolean rare) {
        Object o = ((i & 1) == 0) ? new P() : new Node();
        int r = (o == null) ? 1 : 0;
        r += (o != null) ? 10 : 0;
        if (rare) {
            Late7.touch();
            r += (o instanceof P) ? 100 : 0;
        }
        return r;
    }

    // Optional.of(fresh).get(): the holder goes, the null compare in get() folds, the payload
    // keeps its identity across the transfer.
    static int optionalDeopt(int i, boolean rare) {
        P p = new P();
        p.x = i;
        Optional<P> o = Optional.of(p);
        P q = o.get();
        int r = q.x - i;
        if (rare) {
            Late8.touch();
            r += (q == p) ? 1 : 0;
            r += o.isPresent() ? 10 : 0;
        }
        return r;
    }

    interface Row {
        int run(int i, boolean rare);
    }

    static void row(String name, Row r) {
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += r.run(i, false);
        }
        System.out.println("fact " + name + " " + r.run(WARM, true));
        System.out.println("fact " + name + "-warm " + sum);
    }

    public static void main(String[] args) {
        long edgeWarm = 0;
        for (int i = 0; i < WARM; i++) {
            edgeWarm += boxEdge127(i, false) + boxEdge128(i, false) + boxEdge129(i, false);
        }
        System.out.println("fact box-edge-127 " + boxEdge127(WARM, true));
        System.out.println("fact box-edge-128 " + boxEdge128(WARM, true));
        System.out.println("fact box-edge-129 " + boxEdge129(WARM, true));
        System.out.println("fact box-edge-warm " + edgeWarm);
        row("box-two-webs", J27T1EaRows::boxTwoWebs);
        row("box-and-holder", J27T1EaRows::boxAndHolder);
        row("ref-cmp-deopt", J27T1EaRows::refCmpDeopt);
        row("null-phi", J27T1EaRows::nullPhi);
        row("optional-deopt", J27T1EaRows::optionalDeopt);
        System.out.println("fact late-hits " + lateHits);
        if (Boolean.getBoolean("j27.time")) {
            timeRow("box-two-webs", J27T1EaRows::boxTwoWebs);
            timeRow("box-and-holder", J27T1EaRows::boxAndHolder);
        }
    }

    static void timeRow(String name, Row r) {
        final int ops = 5_000_000;
        long sink = 0;
        for (int i = 0; i < ops; i++) {
            sink += r.run(i, false);
        }
        long t0 = System.nanoTime();
        for (int i = 0; i < ops; i++) {
            sink += r.run(i, false);
        }
        long ns = System.nanoTime() - t0;
        System.out.println("time " + name + " " + (ns / ops) + " ns/op sink=" + sink);
    }
}
