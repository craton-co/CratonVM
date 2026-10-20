// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z1, lane mk (2026-10-12).
//
// The SATB hole an unbarriered slot opens under CONCURRENT marking: on
// --compatible, java.util.LinkedList / TreeMap / LinkedHashMap keep their
// contents in native collection overlays, which are not heap slots, so the
// ZGC SATB pre-write barrier never sees a reference LEAVE one. Moving an
// element from an unscanned collection into an already-scanned one
// (`b.addLast(a.removeFirst())`) hides it from the marker: `a` no longer
// holds it when it is scanned, `b` is never revisited, and the element is
// swept while `b` is live. zr z1/mk records the rows written during the phase
// and re-reads them at mark end (CRATONVM_ZGC_CONC_OVERLAY_REPAIR, default on).
//
// Each mover thread owns a pair of collections and shuttles `n` elements
// between them for `seconds`; the main thread allocates garbage so cycles
// open on their own. Every element carries its id twice; a swept-and-reused
// element reads back wrong.
//
// Output (HotSpot prints the same):
//   ZR1MK-CONC-OVERLAY movers=<m> n=<n> lost=0 corrupt=0 ok
import java.util.LinkedList;
import java.util.Map;
import java.util.TreeMap;

public class Zr1MkConcOverlayMove {
    static final class Elem {
        final int id;
        final int[] payload;
        Elem(int id) {
            this.id = id;
            this.payload = new int[] {id, ~id};
        }
        boolean intact(int want) {
            return id == want && payload.length == 2 && payload[0] == want && payload[1] == ~want;
        }
    }

    public static void main(String[] args) throws Exception {
        int movers = args.length > 0 ? Integer.parseInt(args[0]) : 2;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        int seconds = args.length > 2 ? Integer.parseInt(args[2]) : 20;
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        int[] lost = new int[movers];
        int[] corrupt = new int[movers];
        Thread[] ts = new Thread[movers];
        for (int m = 0; m < movers; m++) {
            final int me = m;
            ts[m] = new Thread(() -> {
                LinkedList<Elem> a = new LinkedList<>();
                LinkedList<Elem> b = new LinkedList<>();
                TreeMap<Integer, Elem> ta = new TreeMap<>();
                TreeMap<Integer, Elem> tb = new TreeMap<>();
                for (int i = 0; i < n; i++) {
                    a.addLast(new Elem(i));
                    ta.put(i, new Elem(i));
                }
                while (System.nanoTime() < end) {
                    for (int k = 0; k < 256; k++) {
                        if (!a.isEmpty()) b.addLast(a.removeFirst());
                        else if (!b.isEmpty()) a.addLast(b.removeFirst());
                        Map.Entry<Integer, Elem> e = ta.pollFirstEntry();
                        if (e != null) tb.put(e.getKey(), e.getValue());
                        else {
                            e = tb.pollFirstEntry();
                            if (e != null) ta.put(e.getKey(), e.getValue());
                        }
                    }
                }
                boolean[] seen = new boolean[n];
                boolean[] seenT = new boolean[n];
                for (LinkedList<Elem> l : java.util.List.of(a, b)) {
                    for (Elem x : l) {
                        if (x == null || x.id < 0 || x.id >= n || !x.intact(x.id)) corrupt[me]++;
                        else seen[x.id] = true;
                    }
                }
                for (TreeMap<Integer, Elem> t : java.util.List.of(ta, tb)) {
                    for (Map.Entry<Integer, Elem> x : t.entrySet()) {
                        Elem v = x.getValue();
                        if (v == null || !v.intact(x.getKey())) corrupt[me]++;
                        else seenT[x.getKey()] = true;
                    }
                }
                for (int i = 0; i < n; i++) {
                    if (!seen[i]) lost[me]++;
                    if (!seenT[i]) lost[me]++;
                }
            });
            ts[m].start();
        }
        Object[] junk = new Object[4096];
        long i = 0;
        while (System.nanoTime() < end) {
            junk[(int) (i++ & 4095)] = new int[64];
        }
        for (Thread t : ts) t.join();
        int l = 0, c = 0;
        for (int m = 0; m < movers; m++) {
            l += lost[m];
            c += corrupt[m];
        }
        System.out.println("ZR1MK-CONC-OVERLAY movers=" + movers + " n=" + n + " lost=" + l + " corrupt=" + c
                + (l == 0 && c == 0 ? " ok" : " BAD"));
    }
}
