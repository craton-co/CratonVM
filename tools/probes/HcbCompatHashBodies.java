// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// hcb (JIT round 16): the `jdk-hash-collections` HotSpot-alignment row. Under
// `--compatible` over a real JDK, HashMap / LinkedHashMap / HashSet /
// LinkedHashSet run the JDK's own bodies, as under `--jdk-only`. Every line
// below must match HotSpot 25 byte for byte in all three arms:
//
//   java -cp . HcbCompatHashBodies
//   cratonvm --compatible --cp . HcbCompatHashBodies
//   cratonvm --compatible --cp . HcbCompatHashBodies   (CRATONVM_COMPAT_ALIGN=-jdk-hash-collections)
//   cratonvm --cp . HcbCompatHashBodies                (--jdk-only)
//
// The rows are what a Rust-side representation of a map can get wrong: the
// order an Integer-keyed map iterates in (the integer overlay modelled it),
// how many times a user key's hashCode / equals is called, a treeified
// collision bin, access order, a structural change inside computeIfAbsent,
// clone and serialization. The timing row (`keyget`) is informative only.

import java.io.*;
import java.util.*;

public class HcbCompatHashBodies {
    static int hashCalls, eqCalls;

    static final class Key implements Serializable {
        final int id;
        final int bucket;
        Key(int id, int bucket) { this.id = id; this.bucket = bucket; }
        @Override public int hashCode() { hashCalls++; return bucket; }
        @Override public boolean equals(Object o) {
            eqCalls++;
            return o instanceof Key k && k.id == id;
        }
        @Override public String toString() { return "K" + id; }
    }

    static void row(String name, Object value) {
        System.out.println(name + ": " + value);
    }

    static List<Integer> sortedIds(Set<Key> ks) {
        List<Integer> ids = new ArrayList<>();
        for (Key k : ks) ids.add(k.id);
        Collections.sort(ids);
        return ids;
    }

    static String counts() {
        String s = "hash=" + hashCalls + " eq=" + eqCalls;
        hashCalls = 0;
        eqCalls = 0;
        return s;
    }

    public static void main(String[] args) throws Exception {
        // 1. Integer keys: iteration order over a resize, removal, views.
        Map<Integer, String> im = new HashMap<>();
        int[] ks = {97, 3, 1024, -5, 17, 65536, 33, 1, 48, 200, 16, 0, -1, 999, 12345};
        for (int k : ks) im.put(k, "v" + k);
        row("int.order", im.keySet());
        im.remove(1024);
        im.remove(-5);
        row("int.afterRemove", im);
        row("int.values", im.values());
        Set<Integer> is = new HashSet<>();
        for (int k : ks) is.add(k * 7);
        row("intset.order", is);
        Iterator<Integer> it = is.iterator();
        it.next();
        it.remove();
        row("intset.afterItrRemove", is + " size=" + is.size());
        try {
            for (Integer k : im.keySet()) if (k == 17) im.put(4242, "x");
            row("int.cme", "none");
        } catch (ConcurrentModificationException e) {
            row("int.cme", e.getClass().getName());
        }
        Map<Character, Integer> cm = new HashMap<>();
        for (char c : "hashcollections".toCharArray()) cm.merge(c, 1, Integer::sum);
        row("char.merge", cm);
        Map<Short, Integer> sm = new HashMap<>();
        sm.put((short) 3, 1);
        row("short.vs.int", sm.get(3) + "/" + sm.get((short) 3));

        // 2. A user key: call counts for each operation.
        Map<Key, Integer> um = new HashMap<>();
        Key[] keys = new Key[40];
        for (int i = 0; i < keys.length; i++) keys[i] = new Key(i, i % 5);
        counts();
        for (Key k : keys) um.put(k, k.id);
        row("user.put", counts());
        int s = 0;
        for (Key k : keys) s += um.get(k);
        row("user.get", s + " " + counts());
        row("user.getEqualCopy", um.get(new Key(7, 2)) + " " + counts());
        row("user.containsKey", um.containsKey(new Key(39, 4)) + " " + counts());
        row("user.remove", um.remove(new Key(12, 2)) + " " + counts());
        row("user.getMissing", um.get(new Key(1000, 1)) + " " + counts());
        row("user.order", um.keySet());
        counts();

        // 3. One bucket, 20 keys: the bin treeifies past 8 (64 buckets after
        // the resizes); lookups walk the tree.
        Map<Key, Integer> tm = new HashMap<>();
        for (int i = 0; i < 20; i++) tm.put(new Key(i, 77), i);
        counts();
        int t = 0;
        for (int i = 0; i < 20; i++) t += tm.get(new Key(i, 77));
        row("tree.get", t);
        // A non-Comparable key's tree order breaks ties by identityHashCode,
        // so the bin's iteration order is not reproducible: print the set.
        row("tree.keys", sortedIds(tm.keySet()));
        for (int i = 0; i < 15; i++) tm.remove(new Key(i, 77));
        row("tree.afterRemove", sortedIds(tm.keySet()) + " size=" + tm.size());
        counts();

        // 4. LinkedHashMap: access order, removeEldestEntry, sequenced views.
        LinkedHashMap<String, Integer> lru = new LinkedHashMap<>(16, 0.75f, true) {
            @Override protected boolean removeEldestEntry(Map.Entry<String, Integer> e) {
                return size() > 4;
            }
        };
        for (String k : new String[] {"a", "b", "c", "d"}) lru.put(k, k.charAt(0) - 'a');
        lru.get("a");
        lru.put("e", 4);
        lru.getOrDefault("c", -1);
        row("lru", lru);
        row("lru.reversed", lru.reversed());
        row("lru.first", lru.firstEntry() + " last=" + lru.lastEntry());
        LinkedHashSet<String> lhs = new LinkedHashSet<>(List.of("z", "y", "x"));
        lhs.addFirst("w");
        lhs.addLast("z");
        row("lhs", lhs + " rev=" + lhs.reversed());

        // 5. computeIfAbsent: a structural change inside the function.
        Map<String, Integer> cmap = new HashMap<>();
        try {
            cmap.computeIfAbsent("k", k -> { cmap.put("other", 1); return 2; });
            row("cia.cme", "none " + cmap);
        } catch (ConcurrentModificationException e) {
            row("cia.cme", e.getClass().getName());
        }
        Map<String, Integer> cm2 = new HashMap<>();
        row("cia.null", cm2.computeIfAbsent("n", k -> null) + " " + cm2.containsKey("n"));
        row("merge.remove", cm2.merge("m", 1, (a, b) -> null) + " " + cm2.merge("m", 5, (a, b) -> null) + " " + cm2);

        // 6. Null keys and values, equals / hashCode / toString, clone.
        HashMap<String, String> nm = new HashMap<>();
        nm.put(null, "nk");
        nm.put("nv", null);
        nm.put("a", "b");
        row("null", nm + " get(null)=" + nm.get(null) + " cv=" + nm.containsValue(null));
        @SuppressWarnings("unchecked")
        HashMap<String, String> cl = (HashMap<String, String>) nm.clone();
        cl.put("c", "d");
        row("clone", nm.size() + "/" + cl.size() + " eq=" + nm.equals(cl) + " h=" + nm.hashCode());
        row("entry.toArray", Arrays.toString(nm.entrySet().toArray()));
        row("copyOf", new ArrayList<>(nm.keySet()) + " " + new HashSet<>(nm.values()));

        // 7. Serialization round trip.
        HashMap<Object, Object> ser = new HashMap<>();
        for (int i = 0; i < 30; i++) ser.put(i % 3 == 0 ? (Object) ("s" + i) : (Object) i, i);
        ser.put(new Key(5, 5), "key");
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) { oos.writeObject(ser); }
        Object back;
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            back = ois.readObject();
        }
        row("ser", bos.size() + " " + back.equals(ser) + " " + ((Map<?, ?>) back).keySet());
        counts();

        // 8. The page's row: a 64-entry map of user keys, `get` in a loop.
        Map<Key, Integer> km = new HashMap<>();
        Key[] kk = new Key[64];
        for (int i = 0; i < 64; i++) { kk[i] = new Key(i, i * 31); km.put(kk[i], i); }
        long sum = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        for (int r = 0; r < 3; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) sum += km.get(kk[i & 63]);
            long dt = System.nanoTime() - t0;
            if (r == 2) System.out.printf("keyget %.1f ns/op%n", dt / (double) iters);
        }
        row("keyget.sum", sum);
    }
}
