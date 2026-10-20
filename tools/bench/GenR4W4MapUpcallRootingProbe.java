// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Generational GC round 4, wave 4, lane rooting2 (2026-09-24): map natives that
 * run a Java upcall ({@code hashCode()}, {@code equals()}, {@code toString()},
 * the mapping function) and then use a Rust local captured before it.
 *
 * <p>Every key and value here ALLOCATES inside {@code hashCode}/{@code equals}/
 * {@code toString}, so under {@code CRATONVM_DBG=gc-stress=250000} a moving
 * young collection lands inside those upcalls. Before the fixes this probe
 * checks, the natives read the segment list, the target, the default value,
 * the mapped value or the forwarded argument slice through the pre-upcall
 * address:
 *
 * <ul>
 *   <li>{@code ConcurrentHashMap.containsValue} and {@code hashCode} scanned every
 *       segment after the first at a vacated address (a miss / a 0 contribution);
 *   <li>{@code ConcurrentHashMap.toString} rendered entries from pre-move copies;
 *   <li>{@code ConcurrentHashMap.computeIfAbsent} returned the pre-put address of
 *       the value it had just stored;
 *   <li>{@code HashMap.getOrDefault} re-read the default BEFORE its
 *       {@code containsKey} re-ask, so the key-absent branch could return a
 *       pre-move default;
 *   <li>{@code LinkedHashMap.putIfAbsent} forwarded the caller's original
 *       argument slice to its insert after the lookup ran {@code equals()}.
 * </ul>
 *
 * <p>Run:
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4W4MapUpcallRootingProbe                       # reference
 *   CRATONVM_DBG=gc-stress=250000 cratonvm -XX:+UseGenerationalGC -Xmx256m \
 *       -cp tools/bench GenR4W4MapUpcallRootingProbe
 * </pre>
 *
 * <p>Expected, identical on both (HotSpot 25 output; every line is computed from
 * values, never from addresses or iteration order):
 * <pre>
 * containsValue hits=40 absentFound=false
 * hashCode matches=true
 * toString entries=2000
 * computeIfAbsent identityMismatches=0 sizeAfter=2200
 * HashMap.getOrDefault defaultMismatches=0
 * LinkedHashMap.putIfAbsent mismatches=0
 * LinkedHashMap.getOrDefault defaultMismatches=0
 * done
 * </pre>
 */
public class GenR4W4MapUpcallRootingProbe {
    static volatile Object sink;

    /** Allocates on every hashCode/equals/toString call. */
    static final class Key {
        final int id;

        Key(int id) {
            this.id = id;
        }

        @Override
        public int hashCode() {
            sink = new byte[512];
            return id * 31 + 7;
        }

        @Override
        public boolean equals(Object o) {
            sink = new int[64];
            return o instanceof Key && ((Key) o).id == id;
        }

        @Override
        public String toString() {
            sink = new long[32];
            return "k" + id;
        }
    }

    static final class Val {
        final int v;

        Val(int v) {
            this.v = v;
        }

        @Override
        public int hashCode() {
            sink = new byte[256];
            return v;
        }

        @Override
        public boolean equals(Object o) {
            sink = new char[128];
            return o instanceof Val && ((Val) o).v == v;
        }

        @Override
        public String toString() {
            sink = new short[64];
            return "v" + v;
        }
    }

    public static void main(String[] args) {
        final int n = 2000;
        ConcurrentHashMap<Key, Val> chm = new ConcurrentHashMap<>();
        for (int i = 0; i < n; i++) {
            chm.put(new Key(i), new Val(i * 7));
        }

        int hits = 0;
        for (int i = 0; i < n; i += 50) {
            if (chm.containsValue(new Val(i * 7))) {
                hits++;
            }
        }
        boolean absentFound = chm.containsValue(new Val(-1));
        System.out.println("containsValue hits=" + hits + " absentFound=" + absentFound);

        int expectedHash = 0;
        for (int i = 0; i < n; i++) {
            expectedHash += (i * 31 + 7) ^ (i * 7);
        }
        System.out.println("hashCode matches=" + (chm.hashCode() == expectedHash));

        String rendered = chm.toString();
        int entries = 0;
        for (int i = 0; i < n; i++) {
            String entry = "k" + i + "=v" + (i * 7);
            int at = rendered.indexOf(entry);
            // Exact token: bounded by '{' / ", " before and ',' / '}' after.
            while (at >= 0) {
                boolean startOk = at == 1 || rendered.startsWith(", ", at - 2);
                int end = at + entry.length();
                boolean endOk = end < rendered.length()
                        && (rendered.charAt(end) == ',' || rendered.charAt(end) == '}');
                if (startOk && endOk) {
                    entries++;
                    break;
                }
                at = rendered.indexOf(entry, at + 1);
            }
        }
        System.out.println("toString entries=" + entries);

        int identityMismatches = 0;
        for (int i = n; i < n + 200; i++) {
            final int id = i;
            Val computed = chm.computeIfAbsent(new Key(id), k -> new Val(id * 7));
            if (computed != chm.get(new Key(id))) {
                identityMismatches++;
            }
        }
        System.out.println("computeIfAbsent identityMismatches=" + identityMismatches
                + " sizeAfter=" + chm.size());

        Map<Key, Val> hm = new HashMap<>();
        for (int i = 0; i < 500; i++) {
            hm.put(new Key(i), new Val(i));
        }
        int hmDefaultMismatches = 0;
        for (int i = 0; i < 500; i++) {
            Val d = new Val(-i);
            if (hm.getOrDefault(new Key(100_000 + i), d) != d) {
                hmDefaultMismatches++;
            }
        }
        System.out.println("HashMap.getOrDefault defaultMismatches=" + hmDefaultMismatches);

        LinkedHashMap<Key, Val> lhm = new LinkedHashMap<>();
        for (int i = 0; i < 300; i++) {
            lhm.put(new Key(i), i % 3 == 0 ? null : new Val(i));
        }
        int lhmMismatches = 0;
        for (int i = 0; i < 600; i++) {
            Val v = new Val(10_000 + i);
            Val before = lhm.get(new Key(i));
            Val prior = lhm.putIfAbsent(new Key(i), v);
            Val now = lhm.get(new Key(i));
            if (before == null) {
                // absent, or present with a null value: putIfAbsent stores v.
                if (prior != null || now != v) {
                    lhmMismatches++;
                }
            } else if (prior != before || now != before) {
                lhmMismatches++;
            }
        }
        System.out.println("LinkedHashMap.putIfAbsent mismatches=" + lhmMismatches);

        int lhmDefaultMismatches = 0;
        for (int i = 0; i < 500; i++) {
            Val d = new Val(-i);
            if (lhm.getOrDefault(new Key(100_000 + i), d) != d) {
                lhmDefaultMismatches++;
            }
        }
        System.out.println("LinkedHashMap.getOrDefault defaultMismatches=" + lhmDefaultMismatches);
        System.out.println("done");
    }
}
