import java.util.*;

/**
 * Differential over the immutable-collection factories and the views that
 * shadow them, for the synthetic-stub retirement wave.
 *
 * The census (schema 4, JDK 25.0.4/linux, compatible mode) reports these
 * triples registered `SyntheticStub` on classes whose image bytecode is
 * concrete, and dispatched on a boot+println workload — a fake running with
 * the real implementation sitting right there:
 *
 *   java/util/Set.copyOf, Set.of(x3..x8), List.of(x1,x3),
 *   java/util/Collections.unmodifiableSet, java/util/ArrayList.subList
 *
 * Run under HotSpot, --real-jdk and --jdk-only and diff the transcripts. A
 * line that differs is a defect; a line that agrees discharges that row.
 *
 * EVERY SECTION IS FENCED. A differential that stops early cannot report a
 * difference, and a truncated transcript reads exactly like a clean one.
 */
public final class ImmutableCollectionsDifferentialProbe {

    static void p(String k, Object v) { System.out.println(k + " = " + v); }

    /** Run one observation, printing the value or the exception's class+message. */
    static void obs(String key, Callable c) {
        try {
            p(key, c.call());
        } catch (Throwable t) {
            p(key, "THREW " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        }
    }

    interface Callable { Object call() throws Exception; }

    /** Fence a whole section so one crash cannot swallow the rest. */
    static void section(String name, Runnable body) {
        System.out.println("--- " + name);
        try {
            body.run();
        } catch (Throwable t) {
            System.out.println("!!! SECTION ABORTED " + name + " : "
                    + t.getClass().getName() + ": " + t.getMessage());
        }
        System.out.println("--- end " + name);
    }

    public static void main(String[] args) {
        section("List.of", () -> {
            obs("List.of()", () -> List.of());
            obs("List.of(a)", () -> List.of("a"));
            obs("List.of(a,b)", () -> List.of("a", "b"));
            obs("List.of(a,b,c)", () -> List.of("a", "b", "c"));
            obs("List.of(1..10)", () -> List.of(1, 2, 3, 4, 5, 6, 7, 8, 9, 10));
            obs("List.of().class", () -> List.of().getClass().getName());
            obs("List.of(a).class", () -> List.of("a").getClass().getName());
            obs("List.of(a,b,c).class", () -> List.of("a", "b", "c").getClass().getName());
            obs("List.of(a,null) NPE", () -> List.of("a", (String) null));
            obs("List.of(a).size", () -> List.of("a").size());
            obs("List.of(a,b,c).get(1)", () -> List.of("a", "b", "c").get(1));
            obs("List.of(a,b,c).indexOf(c)", () -> List.of("a", "b", "c").indexOf("c"));
            obs("List.of(a,b,c).contains(b)", () -> List.of("a", "b", "c").contains("b"));
            obs("List.of(a,b,c).toString", () -> List.of("a", "b", "c").toString());
            obs("List.of add UOE", () -> { List.of("a").add("b"); return "NO THROW"; });
            obs("List.of set UOE", () -> { List.of("a").set(0, "b"); return "NO THROW"; });
            obs("List.of remove UOE", () -> { List.of("a").remove(0); return "NO THROW"; });
            obs("List.of equals ArrayList", () -> List.of("a", "b").equals(new ArrayList<>(Arrays.asList("a", "b"))));
            obs("List.of hashCode == ArrayList", () -> List.of("a", "b").hashCode() == new ArrayList<>(Arrays.asList("a", "b")).hashCode());
            obs("List.of iterator order", () -> {
                StringBuilder sb = new StringBuilder();
                for (Object o : List.of("a", "b", "c")) sb.append(o);
                return sb.toString();
            });
        });

        section("Set.of", () -> {
            obs("Set.of()", () -> Set.of().size());
            obs("Set.of(a).size", () -> Set.of("a").size());
            obs("Set.of(a,b).size", () -> Set.of("a", "b").size());
            obs("Set.of(a,b,c).size", () -> Set.of("a", "b", "c").size());
            obs("Set.of(1..8).size", () -> Set.of(1, 2, 3, 4, 5, 6, 7, 8).size());
            obs("Set.of().class", () -> Set.of().getClass().getName());
            obs("Set.of(a).class", () -> Set.of("a").getClass().getName());
            obs("Set.of(a,b,c).class", () -> Set.of("a", "b", "c").getClass().getName());
            obs("Set.of(a,a) dup IAE", () -> Set.of("a", "a"));
            obs("Set.of(a,b,a) dup IAE", () -> Set.of("a", "b", "a"));
            obs("Set.of(a,null) NPE", () -> Set.of("a", (String) null));
            obs("Set.of(a,b,c).contains(b)", () -> Set.of("a", "b", "c").contains("b"));
            obs("Set.of(a,b,c).contains(z)", () -> Set.of("a", "b", "c").contains("z"));
            obs("Set.of add UOE", () -> { Set.of("a").add("b"); return "NO THROW"; });
            obs("Set.of remove UOE", () -> { Set.of("a").remove("a"); return "NO THROW"; });
            obs("Set.of equals HashSet", () -> Set.of("a", "b").equals(new HashSet<>(Arrays.asList("a", "b"))));
            obs("Set.of hashCode == HashSet", () -> Set.of("a", "b").hashCode() == new HashSet<>(Arrays.asList("a", "b")).hashCode());
        });

        section("Map.of", () -> {
            obs("Map.of().size", () -> Map.of().size());
            obs("Map.of(k,v).size", () -> Map.of("k", "v").size());
            obs("Map.of(k1,v1,k2,v2).size", () -> Map.of("a", "1", "b", "2").size());
            obs("Map.of().class", () -> Map.of().getClass().getName());
            obs("Map.of(k,v).class", () -> Map.of("k", "v").getClass().getName());
            obs("Map.of dup key IAE", () -> Map.of("a", "1", "a", "2"));
            obs("Map.of(null,v) NPE", () -> Map.of((String) null, "1"));
            obs("Map.of get", () -> Map.of("a", "1", "b", "2").get("b"));
            obs("Map.of containsKey", () -> Map.of("a", "1").containsKey("a"));
            obs("Map.of put UOE", () -> { Map.of("a", "1").put("b", "2"); return "NO THROW"; });
            obs("Map.of equals HashMap", () -> {
                Map<String, String> m = new HashMap<>();
                m.put("a", "1");
                return Map.of("a", "1").equals(m);
            });
        });

        section("copyOf", () -> {
            obs("List.copyOf", () -> List.copyOf(Arrays.asList("a", "b")));
            obs("List.copyOf.class", () -> List.copyOf(Arrays.asList("a", "b")).getClass().getName());
            obs("List.copyOf add UOE", () -> { List.copyOf(Arrays.asList("a")).add("b"); return "NO THROW"; });
            obs("List.copyOf(null elem) NPE", () -> List.copyOf(Arrays.asList("a", null)));
            obs("Set.copyOf.size", () -> Set.copyOf(Arrays.asList("a", "b", "a")).size());
            obs("Set.copyOf.class", () -> Set.copyOf(Arrays.asList("a", "b")).getClass().getName());
            obs("Set.copyOf dedups (no IAE)", () -> Set.copyOf(Arrays.asList("a", "a")).size());
            obs("Set.copyOf add UOE", () -> { Set.copyOf(Arrays.asList("a")).add("b"); return "NO THROW"; });
            obs("Map.copyOf.size", () -> {
                Map<String, String> m = new LinkedHashMap<>();
                m.put("a", "1");
                m.put("b", "2");
                return Map.copyOf(m).size();
            });
            obs("copyOf of immutable is identity", () -> {
                List<String> src = List.of("a", "b");
                return List.copyOf(src) == src;
            });
        });

        section("Collections.unmodifiable*", () -> {
            obs("unmodifiableList add UOE", () -> {
                Collections.unmodifiableList(new ArrayList<>(Arrays.asList("a"))).add("b");
                return "NO THROW";
            });
            obs("unmodifiableSet add UOE", () -> {
                Collections.unmodifiableSet(new HashSet<>(Arrays.asList("a"))).add("b");
                return "NO THROW";
            });
            obs("unmodifiableMap put UOE", () -> {
                Collections.unmodifiableMap(new HashMap<String, String>()).put("a", "1");
                return "NO THROW";
            });
            obs("unmodifiableList.class", () -> Collections.unmodifiableList(new ArrayList<>(Arrays.asList("a"))).getClass().getName());
            obs("unmodifiableSet.class", () -> Collections.unmodifiableSet(new HashSet<>(Arrays.asList("a"))).getClass().getName());
            obs("unmodifiableSet is a VIEW", () -> {
                Set<String> backing = new HashSet<>(Arrays.asList("a"));
                Set<String> view = Collections.unmodifiableSet(backing);
                backing.add("b");
                return view.size();
            });
            obs("unmodifiableList is a VIEW", () -> {
                List<String> backing = new ArrayList<>(Arrays.asList("a"));
                List<String> view = Collections.unmodifiableList(backing);
                backing.add("b");
                return view.size();
            });
            obs("unmodifiableSet equals backing", () -> {
                Set<String> backing = new HashSet<>(Arrays.asList("a", "b"));
                return Collections.unmodifiableSet(backing).equals(backing);
            });
        });

        section("ArrayList.subList", () -> {
            obs("subList content", () -> new ArrayList<>(Arrays.asList("a", "b", "c", "d")).subList(1, 3).toString());
            obs("subList size", () -> new ArrayList<>(Arrays.asList("a", "b", "c", "d")).subList(1, 3).size());
            obs("subList.class", () -> new ArrayList<>(Arrays.asList("a", "b")).subList(0, 1).getClass().getName());
            obs("subList is a List", () -> new ArrayList<>(Arrays.asList("a", "b")).subList(0, 1) instanceof List);
            obs("subList write-through", () -> {
                List<String> l = new ArrayList<>(Arrays.asList("a", "b", "c"));
                l.subList(1, 3).set(0, "Z");
                return l.toString();
            });
            obs("subList add grows backing", () -> {
                List<String> l = new ArrayList<>(Arrays.asList("a", "b", "c"));
                l.subList(1, 2).add("Z");
                return l.toString();
            });
            obs("subList CME after backing add", () -> {
                List<String> l = new ArrayList<>(Arrays.asList("a", "b", "c"));
                List<String> sub = l.subList(0, 2);
                l.add("d");
                return sub.size();
            });
            obs("subList(1,1) empty", () -> new ArrayList<>(Arrays.asList("a", "b")).subList(1, 1).isEmpty());
            obs("subList bad range IOOBE", () -> new ArrayList<>(Arrays.asList("a", "b")).subList(0, 5));
            obs("subList equals List.of", () -> new ArrayList<>(Arrays.asList("a", "b", "c")).subList(0, 2).equals(List.of("a", "b")));
        });

        System.out.println("=== PROBE COMPLETE ===");
    }
}
