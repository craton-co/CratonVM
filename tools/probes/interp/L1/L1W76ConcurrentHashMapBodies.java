// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L1:
//
// * `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP` (the `JdkConcurrentHashMap`
//   HotSpot-alignment row, bit 64, default OFF): `--compatible` over a real JDK
//   no longer registers the `ConcurrentHashMap` stand-ins (`native-collections`
//   `register_concurrent_hashmap_natives`, the view / iterator / entry carrier
//   rows, and every retired row of the class prefix), the `Map` /
//   `ConcurrentMap` interface doors hand a CHM receiver to its own bytecode,
//   and the JIT's direct `ConcurrentMap.get` helper declines -- so the JDK 25
//   map runs over its own `table`, as `--jdk-only` already did.
//
// What the stand-ins change (expected from code reading, row off): they keep
// the entries in a VM-side segmented store, so the `order` / `toString` /
// `bulk` rows depend on how well `chm_reorder_by_virtual_bucket` models the
// JDK's bins; `recursive` (the JDK's `ReservationNode` check) and the view /
// iterator / entry classes are the rows most likely to differ.
//
// Run: `cratonvm L1W76ConcurrentHashMapBodies` (jdk-only) and
// `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP=1 cratonvm --compatible
// L1W76ConcurrentHashMapBodies` must both print the block below;
// `cratonvm --compatible L1W76ConcurrentHashMapBodies` (row off) is the
// before. Also with `--nojit` and with the JIT (the `ConcurrentMap.get` door
// is the JIT's direct-helper site: `door-loop`).
//
// The expected block is DERIVED from the JDK 25 sources (ConcurrentHashMap's
// bins: `spread(h) = h ^ h >>> 16`, index `h & (n - 1)`, tail append within a
// bin, table 16 for these sizes), not yet captured from the host oracle:
// refresh it from the host's HotSpot run before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// order: keys=[2, 18, 3, 35, 67, 100] values=[v2, v18, v3, v35, v67, v100]
// toString: {2=v2, 18=v18, 3=v3, 35=v35, 67=v67, 100=v100}
// counts: size=6 mappingCount=6 isEmpty=false
// views: keySet=java.util.concurrent.ConcurrentHashMap$KeySetView values=java.util.concurrent.ConcurrentHashMap$ValuesView entrySet=java.util.concurrent.ConcurrentHashMap$EntrySetView
// iterators: key=java.util.concurrent.ConcurrentHashMap$KeyIterator value=java.util.concurrent.ConcurrentHashMap$ValueIterator entry=java.util.concurrent.ConcurrentHashMap$EntryIterator entryClass=java.util.concurrent.ConcurrentHashMap$MapEntry
// enumerations: keys=java.util.concurrent.ConcurrentHashMap$KeyIterator [2, 18, 3, 35, 67, 100] elements=java.util.concurrent.ConcurrentHashMap$ValueIterator [v2, v18, v3, v35, v67, v100]
// bulk: forEach=2,18,3,35,67,100, reduceValues=v2|v18|v3|v35|v67|v100 search=v35
// map-door: get=v35 containsKey=true size=6 values=[v2, v18, v3, v35, v67, v100] hashMap={2=v2, 18=v18, 3=v3, 35=v35, 67=v67, 100=v100} equals=true
// concurrent-map-door: putIfAbsent=null again=v5 getOrDefault=dflt remove=true replace=v3 replace3=true after={2=v2, 18=v18, 3=v3, 35=v35, 67=v67, 100=v100}
// door-loop: hits=600000
// recursive: java.lang.IllegalStateException: Recursive update size=0 containsKey=false
// compute-merge: merged=3 after={b=11}
// iterator-remove: size=5 contains3=false
// entry-setValue: old=v2 now=z2
// newKeySet: [a, b] class=java.util.concurrent.ConcurrentHashMap$KeySetView contains=true addAgain=false
// keySet(V): {7=dflt, 23=dflt} mapped=dflt
// null: put=java.lang.NullPointerException message=null get=java.lang.NullPointerException
// serialization: {a=1, b=2, c=3} equals=true class=java.util.concurrent.ConcurrentHashMap
// stress: size=4001 hits=4000 sum=7998000
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

public class L1W76ConcurrentHashMapBodies {
    interface Body {
        String run() throws Exception;
    }

    /** One row: its label, then the body's answer or what it threw. */
    static void row(String label, Body body) {
        String out;
        try {
            out = body.run();
        } catch (Throwable t) {
            out = "threw " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + ": " + out);
    }

    /** 100, 3, 35, 67, 2, 18: bins 4, 3, 3, 3, 2, 2 of a 16-bin table. */
    static ConcurrentHashMap<Integer, String> sample() {
        ConcurrentHashMap<Integer, String> m = new ConcurrentHashMap<>();
        for (int k : new int[] {100, 3, 35, 67, 2, 18}) {
            m.put(k, "v" + k);
        }
        return m;
    }

    static <E> List<E> listOf(Iterator<E> it) {
        List<E> out = new ArrayList<>();
        while (it.hasNext()) {
            out.add(it.next());
        }
        return out;
    }

    /** Through the interface: an `invokeinterface ConcurrentMap.get` site. */
    static long doorLoop(ConcurrentMap<Integer, String> cm) {
        long hits = 0;
        int[] keys = {2, 18, 3, 35, 67, 100};
        for (int r = 0; r < 100_000; r++) {
            for (int k : keys) {
                if (cm.get(k) != null) {
                    hits++;
                }
            }
        }
        return hits;
    }

    public static void main(String[] args) throws Exception {
        final ConcurrentHashMap<Integer, String> m = sample();
        row("order", () -> "keys=" + listOf(m.keySet().iterator()) + " values=" + listOf(m.values().iterator()));
        row("toString", () -> m.toString());
        row("counts", () -> "size=" + m.size() + " mappingCount=" + m.mappingCount() + " isEmpty=" + m.isEmpty());
        row("views", () -> "keySet=" + m.keySet().getClass().getName()
                + " values=" + m.values().getClass().getName()
                + " entrySet=" + m.entrySet().getClass().getName());
        row("iterators", () -> "key=" + m.keySet().iterator().getClass().getName()
                + " value=" + m.values().iterator().getClass().getName()
                + " entry=" + m.entrySet().iterator().getClass().getName()
                + " entryClass=" + m.entrySet().iterator().next().getClass().getName());
        row("enumerations", () -> "keys=" + m.keys().getClass().getName() + " " + Collections.list(m.keys())
                + " elements=" + m.elements().getClass().getName() + " " + Collections.list(m.elements()));
        row("bulk", () -> {
            StringBuilder sb = new StringBuilder();
            m.forEach(Long.MAX_VALUE, (k, v) -> sb.append(k).append(','));
            String reduced = m.reduceValues(Long.MAX_VALUE, (a, b) -> a + "|" + b);
            String found = m.search(Long.MAX_VALUE, (k, v) -> k > 30 ? v : null);
            return "forEach=" + sb + " reduceValues=" + reduced + " search=" + found;
        });
        row("map-door", () -> {
            Map<Integer, String> asMap = m;
            return "get=" + asMap.get(35) + " containsKey=" + asMap.containsKey(67) + " size=" + asMap.size()
                    + " values=" + new ArrayList<>(asMap.values()) + " hashMap=" + new HashMap<>(asMap)
                    + " equals=" + asMap.equals(new HashMap<>(asMap));
        });
        row("concurrent-map-door", () -> {
            ConcurrentMap<Integer, String> cm = m;
            String first = cm.putIfAbsent(5, "v5");
            String again = cm.putIfAbsent(5, "w5");
            String dflt = cm.getOrDefault(99, "dflt");
            boolean removed = cm.remove(5, "v5");
            String replaced = cm.replace(3, "V3");
            boolean back = cm.replace(3, "V3", "v3");
            return "putIfAbsent=" + first + " again=" + again + " getOrDefault=" + dflt + " remove=" + removed
                    + " replace=" + replaced + " replace3=" + back + " after=" + m;
        });
        row("door-loop", () -> "hits=" + doorLoop(m));
        row("recursive", () -> {
            ConcurrentHashMap<Integer, Integer> r = new ConcurrentHashMap<>();
            String thrown;
            try {
                // 1 and 17 share bin 1 of the 16-bin table `computeIfAbsent` makes.
                r.computeIfAbsent(1, k -> r.computeIfAbsent(17, k2 -> 17));
                thrown = "none";
            } catch (IllegalStateException e) {
                thrown = e.getClass().getName() + ": " + e.getMessage();
            }
            return thrown + " size=" + r.size() + " containsKey=" + r.containsKey(1);
        });
        row("compute-merge", () -> {
            ConcurrentHashMap<String, Integer> m2 = new ConcurrentHashMap<>();
            for (int i = 0; i < 3; i++) {
                m2.merge("a", 1, Integer::sum);
            }
            int merged = m2.get("a");
            m2.compute("b", (k, v) -> v == null ? 10 : v + 1);
            m2.compute("b", (k, v) -> v == null ? 10 : v + 1);
            m2.computeIfPresent("a", (k, v) -> null);
            return "merged=" + merged + " after=" + m2;
        });
        row("iterator-remove", () -> {
            ConcurrentHashMap<Integer, String> c = new ConcurrentHashMap<>(m);
            for (Iterator<Integer> it = c.keySet().iterator(); it.hasNext(); ) {
                if (it.next() == 3) {
                    it.remove();
                }
            }
            return "size=" + c.size() + " contains3=" + c.containsKey(3);
        });
        row("entry-setValue", () -> {
            ConcurrentHashMap<Integer, String> c = new ConcurrentHashMap<>(m);
            String old = null;
            for (Map.Entry<Integer, String> e : c.entrySet()) {
                if (e.getKey() == 2) {
                    old = e.setValue("z2");
                }
            }
            return "old=" + old + " now=" + c.get(2);
        });
        row("newKeySet", () -> {
            Set<String> ks = ConcurrentHashMap.newKeySet();
            ks.add("b");
            ks.add("a");
            boolean again = ks.add("a");
            return ks + " class=" + ks.getClass().getName() + " contains=" + ks.contains("a") + " addAgain=" + again;
        });
        row("keySet(V)", () -> {
            ConcurrentHashMap<Integer, String> m3 = new ConcurrentHashMap<>();
            ConcurrentHashMap.KeySetView<Integer, String> kv = m3.keySet("dflt");
            kv.add(7);
            kv.add(23);
            return m3 + " mapped=" + kv.getMappedValue();
        });
        row("null", () -> {
            String put;
            try {
                m.put(null, "x");
                put = "none";
            } catch (NullPointerException e) {
                put = e.getClass().getName() + " message=" + e.getMessage();
            }
            String get;
            try {
                m.get(null);
                get = "none";
            } catch (NullPointerException e) {
                get = e.getClass().getName();
            }
            return "put=" + put + " get=" + get;
        });
        row("serialization", () -> {
            ConcurrentHashMap<String, Integer> s = new ConcurrentHashMap<>();
            s.put("c", 3);
            s.put("a", 1);
            s.put("b", 2);
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
                out.writeObject(s);
            }
            Object back;
            try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
                back = in.readObject();
            }
            return back + " equals=" + back.equals(s) + " class=" + back.getClass().getName();
        });
        row("stress", () -> {
            ConcurrentHashMap<Object, Integer> counts = new ConcurrentHashMap<>();
            Thread[] ts = new Thread[4];
            for (int t = 0; t < ts.length; t++) {
                final int base = t * 1000;
                ts[t] = new Thread(() -> {
                    for (int i = 0; i < 1000; i++) {
                        counts.put(base + i, i);
                        counts.merge("hits", 1, Integer::sum);
                    }
                });
                ts[t].start();
            }
            for (Thread t : ts) {
                t.join();
            }
            long sum = 0;
            for (Object k : counts.keySet()) {
                if (k instanceof Integer) {
                    sum += (Integer) k;
                }
            }
            return "size=" + counts.size() + " hits=" + counts.get("hits") + " sum=" + sum;
        });
        System.out.println("done");
    }
}
