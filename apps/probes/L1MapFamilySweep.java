import java.io.*;
import java.util.*;

/** L1 §10 item 3 — the whole `java/util/HashMap` + views surface, one row per
 *  registered native, so precondition 4 (`invocations > 0` per triple) is
 *  MEASURED for the family rather than waived.
 *
 *  The census of `--dump-native-registry --explain-jdk-only` on this VM lists
 *  98 distinct §1.4 shadows over seven classes — `java/util/HashMap` and its
 *  `$KeySet`, `$Values`, `$EntrySet`, `$KeyIterator`, `$ValueIterator`,
 *  `$EntryIterator` and `$Node` — every one of them bucket A or B, i.e. the
 *  image's own class carries `Code` for it. Three earlier probe runs reached
 *  only 29 of the 98; this reaches the rest, and reaches them through ORDINARY
 *  Java so the comparison against HotSpot is a behaviour comparison and not a
 *  reflection dump.
 *
 *  Every row is isolated: a throw is printed as its class name and the sweep
 *  continues, because the failure this exists to catch is SILENT (a view that
 *  reads empty) and one loud NullPointerException must not hide the twenty
 *  quiet rows behind it.
 *
 *  Iteration order is printed RAW for the three-key maps. HotSpot's `HashMap`
 *  order is a pure function of the keys' hashes and the table length, so it is
 *  an observable this VM either matches or does not.
 */
public final class L1MapFamilySweep {
    static int rows = 0;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String tag, Body b) {
        rows++;
        String v;
        try {
            v = render(b.run());
        } catch (Throwable t) {
            v = "THREW " + t.getClass().getName();
        }
        System.out.println(tag + " |" + v + "|");
    }

    static String render(Object o) {
        if (o == null) {
            return "null";
        }
        if (o instanceof Object[]) {
            Object[] a = (Object[]) o;
            List<String> s = new ArrayList<>();
            for (Object x : a) {
                s.add(String.valueOf(x));
            }
            Collections.sort(s);
            return "[" + a.length + "]" + o.getClass().getComponentType().getName() + s;
        }
        return String.valueOf(o);
    }

    static Map<String, Integer> abc() {
        Map<String, Integer> m = new HashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        return m;
    }

    static List<String> sorted(Collection<?> c) {
        List<String> s = new ArrayList<>();
        for (Object o : c) {
            s.add(String.valueOf(o));
        }
        Collections.sort(s);
        return s;
    }

    static List<String> raw(Iterable<?> c) {
        List<String> s = new ArrayList<>();
        for (Object o : c) {
            s.add(String.valueOf(o));
        }
        return s;
    }

    static List<String> splitSorted(String bracketed) {
        String inner = bracketed.substring(1, bracketed.length() - 1);
        if (inner.isEmpty()) {
            return new ArrayList<>();
        }
        List<String> parts = new ArrayList<>(Arrays.asList(inner.split(", ")));
        Collections.sort(parts);
        return parts;
    }

    // ---- java/util/HashMap itself ------------------------------------------

    static void mapSurface() {
        row("M.ctor0.size", () -> new HashMap<String, Integer>().size());
        row("M.ctorI.size", () -> new HashMap<String, Integer>(64).size());
        row("M.ctorIF.size", () -> new HashMap<String, Integer>(64, 0.5f).size());
        row("M.ctorMap.entries", () -> sorted(new HashMap<>(abc()).entrySet()));
        row("M.ctorI.negative", () -> new HashMap<String, Integer>(-1).size());
        row("M.ctorIF.nanLoad", () -> new HashMap<String, Integer>(16, Float.NaN).size());
        row("M.ctorMap.null", () -> new HashMap<String, Integer>((Map<String, Integer>) null).size());

        row("M.put.returnsOld", () -> abc().put("a", 9));
        row("M.put.newKey", () -> abc().put("d", 4));
        row("M.put.nullKey", () -> {
            Map<String, Integer> m = abc();
            m.put(null, 0);
            return sorted(m.keySet());
        });
        row("M.get.hit", () -> abc().get("b"));
        row("M.get.miss", () -> abc().get("z"));
        row("M.get.null", () -> abc().get(null));
        row("M.getOrDefault.hit", () -> abc().getOrDefault("b", -1));
        row("M.getOrDefault.miss", () -> abc().getOrDefault("z", -1));
        row("M.containsKey", () -> abc().containsKey("b") + "/" + abc().containsKey("z"));
        row("M.containsValue", () -> abc().containsValue(2) + "/" + abc().containsValue(99));
        row("M.size", () -> abc().size());
        row("M.isEmpty", () -> abc().isEmpty() + "/" + new HashMap<>().isEmpty());
        row("M.clear", () -> {
            Map<String, Integer> m = abc();
            m.clear();
            return m.size() + "/" + m.isEmpty();
        });
        row("M.remove1.hit", () -> {
            Map<String, Integer> m = abc();
            return m.remove("b") + "/" + m.size();
        });
        row("M.remove1.miss", () -> {
            Map<String, Integer> m = abc();
            return m.remove("z") + "/" + m.size();
        });
        row("M.remove2.match", () -> {
            Map<String, Integer> m = abc();
            return m.remove("b", 2) + "/" + m.size();
        });
        row("M.remove2.mismatch", () -> {
            Map<String, Integer> m = abc();
            return m.remove("b", 9) + "/" + m.size();
        });
        row("M.replace2.hit", () -> {
            Map<String, Integer> m = abc();
            return m.replace("b", 9) + "/" + m.get("b");
        });
        row("M.replace2.miss", () -> {
            Map<String, Integer> m = abc();
            return m.replace("z", 9) + "/" + m.size();
        });
        row("M.replace3.match", () -> {
            Map<String, Integer> m = abc();
            return m.replace("b", 2, 9) + "/" + m.get("b");
        });
        row("M.replace3.mismatch", () -> {
            Map<String, Integer> m = abc();
            return m.replace("b", 8, 9) + "/" + m.get("b");
        });
        row("M.putIfAbsent.present", () -> {
            Map<String, Integer> m = abc();
            return m.putIfAbsent("a", 9) + "/" + m.get("a");
        });
        row("M.putIfAbsent.absent", () -> {
            Map<String, Integer> m = abc();
            return m.putIfAbsent("d", 4) + "/" + m.get("d");
        });
        row("M.putAll", () -> {
            Map<String, Integer> m = abc();
            m.putAll(Map.of("d", 4));
            return sorted(m.keySet());
        });
        row("M.putAll.null", () -> {
            Map<String, Integer> m = abc();
            m.putAll(null);
            return m.size();
        });
        row("M.replaceAll", () -> {
            Map<String, Integer> m = abc();
            m.replaceAll((k, v) -> v * 10);
            return sorted(m.values());
        });
        row("M.replaceAll.null", () -> {
            Map<String, Integer> m = abc();
            m.replaceAll(null);
            return m.size();
        });
        row("M.compute.update", () -> {
            Map<String, Integer> m = abc();
            return m.compute("a", (k, v) -> v + 10) + "/" + m.get("a");
        });
        row("M.compute.remove", () -> {
            Map<String, Integer> m = abc();
            return m.compute("a", (k, v) -> null) + "/" + m.size();
        });
        row("M.compute.insert", () -> {
            Map<String, Integer> m = abc();
            return m.compute("z", (k, v) -> 26) + "/" + m.size();
        });
        row("M.compute.null", () -> abc().compute("a", null));
        row("M.computeIfAbsent.present", () -> {
            Map<String, Integer> m = abc();
            return m.computeIfAbsent("a", k -> 99) + "/" + m.get("a");
        });
        row("M.computeIfAbsent.absent", () -> {
            Map<String, Integer> m = abc();
            return m.computeIfAbsent("z", k -> 26) + "/" + m.size();
        });
        row("M.computeIfAbsent.nullResult", () -> {
            Map<String, Integer> m = abc();
            return m.computeIfAbsent("z", k -> null) + "/" + m.size();
        });
        row("M.computeIfPresent.present", () -> {
            Map<String, Integer> m = abc();
            return m.computeIfPresent("a", (k, v) -> v + 10) + "/" + m.get("a");
        });
        row("M.computeIfPresent.absent", () -> {
            Map<String, Integer> m = abc();
            return m.computeIfPresent("z", (k, v) -> 1) + "/" + m.size();
        });
        row("M.merge.present", () -> {
            Map<String, Integer> m = abc();
            return m.merge("a", 10, Integer::sum) + "/" + m.get("a");
        });
        row("M.merge.absent", () -> {
            Map<String, Integer> m = abc();
            return m.merge("z", 10, Integer::sum) + "/" + m.get("z");
        });
        row("M.merge.nullResult", () -> {
            Map<String, Integer> m = abc();
            return m.merge("a", 10, (x, y) -> null) + "/" + m.size();
        });
        row("M.merge.nullValue", () -> abc().merge("a", null, Integer::sum));
        row("M.forEach", () -> {
            List<String> seen = new ArrayList<>();
            abc().forEach((k, v) -> seen.add(k + "=" + v));
            Collections.sort(seen);
            return seen;
        });
        row("M.forEach.null", () -> {
            abc().forEach(null);
            return "no-throw";
        });
        row("M.equals.same", () -> abc().equals(abc()));
        row("M.equals.diff", () -> abc().equals(Map.of("a", 1)));
        row("M.equals.nonMap", () -> abc().equals("x"));
        row("M.hashCode.stable", () -> abc().hashCode() == abc().hashCode());
        row("M.hashCode.value", () -> abc().hashCode());
        row("M.toString", () -> splitSorted(abc().toString()));
        row("M.toString.empty", () -> new HashMap<>().toString());
        row("M.order.keys", () -> raw(abc().keySet()));
        row("M.order.entries", () -> raw(abc().entrySet()));
        row("M.serialize.roundTrip", () -> {
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
                oo.writeObject(new HashMap<>(abc()));
            }
            Object back;
            try (ObjectInputStream oi = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray()))) {
                back = oi.readObject();
            }
            return back.getClass().getName() + "/" + sorted(((Map<?, ?>) back).entrySet());
        });
    }

    // ---- the three views, one body each ------------------------------------

    static void keySetSurface() {
        row("KS.size", () -> abc().keySet().size());
        row("KS.isEmpty", () -> abc().keySet().isEmpty() + "/" + new HashMap<>().keySet().isEmpty());
        row("KS.contains", () -> abc().keySet().contains("b") + "/" + abc().keySet().contains("z"));
        row("KS.containsAll", () -> abc().keySet().containsAll(List.of("a", "b"))
                + "/" + abc().keySet().containsAll(List.of("a", "z")));
        row("KS.iterator", () -> raw(abc().keySet()));
        row("KS.itr.hasNextOnEmpty", () -> new HashMap<String, Integer>().keySet().iterator().hasNext());
        row("KS.itr.nextPastEnd", () -> new HashMap<String, Integer>().keySet().iterator().next());
        row("KS.itr.remove", () -> {
            Map<String, Integer> m = abc();
            Iterator<String> it = m.keySet().iterator();
            it.next();
            it.remove();
            return m.size() + "/" + m.keySet().size();
        });
        row("KS.itr.removeBeforeNext", () -> {
            abc().keySet().iterator().remove();
            return "no-throw";
        });
        row("KS.remove", () -> {
            Map<String, Integer> m = abc();
            return m.keySet().remove("b") + "/" + m.size();
        });
        row("KS.removeMiss", () -> {
            Map<String, Integer> m = abc();
            return m.keySet().remove("z") + "/" + m.size();
        });
        row("KS.removeAll", () -> {
            Map<String, Integer> m = abc();
            return m.keySet().removeAll(List.of("a", "b")) + "/" + m.size();
        });
        row("KS.retainAll", () -> {
            Map<String, Integer> m = abc();
            return m.keySet().retainAll(List.of("a")) + "/" + m.size();
        });
        row("KS.removeIf", () -> {
            Map<String, Integer> m = abc();
            return m.keySet().removeIf(k -> k.equals("a")) + "/" + m.size();
        });
        row("KS.clear", () -> {
            Map<String, Integer> m = abc();
            m.keySet().clear();
            return m.size();
        });
        row("KS.add", () -> abc().keySet().add("d"));
        row("KS.addAll", () -> abc().keySet().addAll(List.of("d")));
        row("KS.toArray", () -> abc().keySet().toArray());
        row("KS.toArrayTyped", () -> abc().keySet().toArray(new String[0]));
        row("KS.toArrayGen", () -> abc().keySet().toArray(String[]::new));
        row("KS.toString", () -> splitSorted(abc().keySet().toString()));
        row("KS.hashCode.stable", () -> abc().keySet().hashCode() == abc().keySet().hashCode());
        row("KS.equals", () -> abc().keySet().equals(Set.of("a", "b", "c")));
        row("KS.forEach", () -> {
            List<String> s = new ArrayList<>();
            abc().keySet().forEach(s::add);
            Collections.sort(s);
            return s;
        });
        row("KS.stream", () -> abc().keySet().stream().sorted().toList());
        row("KS.spliterator.size", () -> abc().keySet().spliterator().estimateSize());
        row("KS.spliterator.walk", () -> {
            List<String> s = new ArrayList<>();
            abc().keySet().spliterator().forEachRemaining(s::add);
            Collections.sort(s);
            return s;
        });
        row("KS.live.afterPut", () -> {
            Map<String, Integer> m = abc();
            Set<String> ks = m.keySet();
            m.put("d", 4);
            return sorted(ks);
        });
    }

    static void valuesSurface() {
        row("V.size", () -> abc().values().size());
        row("V.isEmpty", () -> abc().values().isEmpty() + "/" + new HashMap<>().values().isEmpty());
        row("V.contains", () -> abc().values().contains(2) + "/" + abc().values().contains(99));
        row("V.iterator", () -> raw(abc().values()));
        row("V.itr.remove", () -> {
            Map<String, Integer> m = abc();
            Iterator<Integer> it = m.values().iterator();
            it.next();
            it.remove();
            return m.size();
        });
        row("V.itr.nextPastEnd", () -> new HashMap<String, Integer>().values().iterator().next());
        row("V.remove", () -> {
            Map<String, Integer> m = abc();
            return m.values().remove(2) + "/" + m.size();
        });
        row("V.removeIf", () -> {
            Map<String, Integer> m = abc();
            return m.values().removeIf(v -> v == 2) + "/" + m.size();
        });
        row("V.clear", () -> {
            Map<String, Integer> m = abc();
            m.values().clear();
            return m.size();
        });
        row("V.toArray", () -> abc().values().toArray());
        row("V.toArrayTyped", () -> abc().values().toArray(new Integer[0]));
        row("V.toArrayGen", () -> abc().values().toArray(Integer[]::new));
        row("V.toString", () -> splitSorted(abc().values().toString()));
        row("V.forEach", () -> {
            List<Integer> s = new ArrayList<>();
            abc().values().forEach(s::add);
            Collections.sort(s);
            return s;
        });
        row("V.stream", () -> abc().values().stream().sorted().toList());
        row("V.spliterator.size", () -> abc().values().spliterator().estimateSize());
        row("V.spliterator.walk", () -> {
            List<Integer> s = new ArrayList<>();
            abc().values().spliterator().forEachRemaining(s::add);
            Collections.sort(s);
            return s;
        });
        row("V.live.afterPut", () -> {
            Map<String, Integer> m = abc();
            Collection<Integer> vs = m.values();
            m.put("d", 4);
            return sorted(vs);
        });
    }

    static void entrySetSurface() {
        row("ES.size", () -> abc().entrySet().size());
        row("ES.isEmpty", () -> abc().entrySet().isEmpty() + "/" + new HashMap<>().entrySet().isEmpty());
        row("ES.contains", () -> abc().entrySet().contains(Map.entry("a", 1))
                + "/" + abc().entrySet().contains(Map.entry("a", 9)));
        row("ES.containsAll", () -> abc().entrySet().containsAll(List.of(Map.entry("a", 1), Map.entry("b", 2))));
        row("ES.iterator", () -> raw(abc().entrySet()));
        row("ES.itr.nextPastEnd", () -> new HashMap<String, Integer>().entrySet().iterator().next());
        row("ES.itr.remove", () -> {
            Map<String, Integer> m = abc();
            Iterator<Map.Entry<String, Integer>> it = m.entrySet().iterator();
            it.next();
            it.remove();
            return m.size();
        });
        row("ES.itr.setValue", () -> {
            Map<String, Integer> m = abc();
            for (Map.Entry<String, Integer> e : m.entrySet()) {
                if (e.getKey().equals("a")) {
                    e.setValue(99);
                }
            }
            return m.get("a");
        });
        row("ES.itr.setValue.returnsOld", () -> {
            Map<String, Integer> m = abc();
            Object old = null;
            for (Map.Entry<String, Integer> e : m.entrySet()) {
                if (e.getKey().equals("a")) {
                    old = e.setValue(99);
                }
            }
            return old;
        });
        row("ES.remove", () -> {
            Map<String, Integer> m = abc();
            return m.entrySet().remove(Map.entry("a", 1)) + "/" + m.size();
        });
        row("ES.removeAll", () -> {
            Map<String, Integer> m = abc();
            return m.entrySet().removeAll(List.of(Map.entry("a", 1))) + "/" + m.size();
        });
        row("ES.retainAll", () -> {
            Map<String, Integer> m = abc();
            return m.entrySet().retainAll(List.of(Map.entry("a", 1))) + "/" + m.size();
        });
        row("ES.removeIf", () -> {
            Map<String, Integer> m = abc();
            return m.entrySet().removeIf(e -> e.getKey().equals("a")) + "/" + m.size();
        });
        row("ES.clear", () -> {
            Map<String, Integer> m = abc();
            m.entrySet().clear();
            return m.size();
        });
        row("ES.add", () -> abc().entrySet().add(Map.entry("d", 4)));
        row("ES.addAll", () -> abc().entrySet().addAll(List.of(Map.entry("d", 4))));
        row("ES.toArray", () -> abc().entrySet().toArray());
        row("ES.toArrayObj", () -> abc().entrySet().toArray(new Object[0]));
        row("ES.toArrayEntry", () -> abc().entrySet().toArray(new Map.Entry[0]));
        row("ES.toArrayString", () -> abc().entrySet().toArray(new String[0]));
        row("ES.toArrayGen", () -> abc().entrySet().toArray(Object[]::new));
        row("ES.toString", () -> splitSorted(abc().entrySet().toString()));
        row("ES.hashCode.stable", () -> abc().entrySet().hashCode() == abc().entrySet().hashCode());
        row("ES.equals", () -> abc().entrySet().equals(abc().entrySet()));
        row("ES.forEach", () -> {
            List<String> s = new ArrayList<>();
            abc().entrySet().forEach(e -> s.add(e.getKey() + "=" + e.getValue()));
            Collections.sort(s);
            return s;
        });
        row("ES.stream", () -> abc().entrySet().stream().map(e -> e.getKey() + "=" + e.getValue()).sorted().toList());
        row("ES.spliterator.size", () -> abc().entrySet().spliterator().estimateSize());
        row("ES.spliterator.walk", () -> {
            List<String> s = new ArrayList<>();
            abc().entrySet().spliterator().forEachRemaining(e -> s.add(e.getKey() + "=" + e.getValue()));
            Collections.sort(s);
            return s;
        });
        row("ES.intoArrayList", () -> new ArrayList<>(abc().entrySet()).size());
        row("ES.intoHashSet", () -> new HashSet<>(abc().entrySet()).size());
        row("ES.live.afterPut", () -> {
            Map<String, Integer> m = abc();
            Set<Map.Entry<String, Integer>> es = m.entrySet();
            m.put("d", 4);
            return es.size();
        });
        row("ES.node.setValue", () -> {
            Map<String, Integer> m = abc();
            Map.Entry<String, Integer> e = m.entrySet().iterator().next();
            Object old = e.setValue(77);
            return old + "/" + m.get(e.getKey());
        });
    }

    static void stress() {
        row("X.cme.keySet", () -> {
            Map<String, Integer> m = abc();
            for (String k : m.keySet()) {
                m.put("z" + k, 1);
            }
            return "no-throw";
        });
        row("X.cme.entrySet", () -> {
            Map<String, Integer> m = abc();
            for (Map.Entry<String, Integer> e : m.entrySet()) {
                m.remove("c");
            }
            return "no-throw";
        });
        row("X.cme.values", () -> {
            Map<String, Integer> m = abc();
            for (Integer v : m.values()) {
                m.put("q", 9);
            }
            return "no-throw";
        });
        row("X.resize.100", () -> {
            Map<Integer, Integer> m = new HashMap<>();
            for (int i = 0; i < 100; i++) {
                m.put(i, i * 2);
            }
            long sum = 0;
            for (Map.Entry<Integer, Integer> e : m.entrySet()) {
                sum += e.getValue();
            }
            return m.size() + "/" + sum + "/" + m.keySet().size() + "/" + m.entrySet().toArray().length;
        });
        row("X.collision.chain", () -> {
            Map<Key, Integer> m = new HashMap<>();
            for (int i = 0; i < 12; i++) {
                m.put(new Key(i), i);
            }
            return m.size() + "/" + m.entrySet().toArray().length + "/" + m.values().toArray().length;
        });
    }

    /** Every instance hashes to the same bucket, so the map must build a chain
     *  and, past `TREEIFY_THRESHOLD`, a tree. */
    static final class Key implements Comparable<Key> {
        final int id;

        Key(int id) {
            this.id = id;
        }

        @Override public int hashCode() {
            return 7;
        }

        @Override public boolean equals(Object o) {
            return o instanceof Key && ((Key) o).id == id;
        }

        @Override public int compareTo(Key o) {
            return Integer.compare(id, o.id);
        }

        @Override public String toString() {
            return "K" + id;
        }
    }

    static void section(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable t) {
            System.out.println(name + ".SECTION |ABORTED " + t.getClass().getName() + "|");
        }
    }

    public static void main(String[] a) {
        section("M", L1MapFamilySweep::mapSurface);
        section("KS", L1MapFamilySweep::keySetSurface);
        section("V", L1MapFamilySweep::valuesSurface);
        section("ES", L1MapFamilySweep::entrySetSurface);
        section("X", L1MapFamilySweep::stress);
        System.out.println("rows " + rows);
        System.out.println("DONE L1MapFamilySweep");
    }

    private L1MapFamilySweep() {
    }
}
