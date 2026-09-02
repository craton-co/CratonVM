import java.util.*;

/**
 * `Collections.empty{List,Set,Map}()` return PROCESS-GLOBAL singletons. If they
 * are mutable, one caller's mistake corrupts every other caller in the VM.
 *
 * That is not hypothetical here. `ensure_collections_empty_singletons` records
 * what happened when these were seeded as ordinary mutable `ArrayList` /
 * `HashSet` / `HashMap` synthetics: kotlin-reflect's shaded protobuf
 * (`SmallSortedMap.ensureEntryArrayMutable`) tests `instanceof ArrayList` to
 * decide whether it must replace its `Collections.emptyList()` placeholder
 * before inserting, skipped the replacement, and the insert SUCCEEDED — after
 * which every `Collections.emptyList()` in the process contained a phantom
 * entry and JUnit's `ReflectionUtils.findFields` failed on every class.
 *
 * The fix seeded the real `Collections$Empty*` classes, whose mutators run real
 * bytecode and throw. It falls back to the mutable synthetics when those
 * classes are unavailable — which is `--synthetic-jdk`, so that mode kept the
 * old behaviour.
 *
 * Every row is a fixed string. `mutated=` is the one that matters: it asks a
 * SECOND, independently obtained singleton whether the first one's write is
 * visible, which is the process-global corruption rather than a failed guard.
 */
public final class EmptySingletonImmutable {
    public static void main(String[] args) {
        System.out.println("emptyList  | " + list());
        System.out.println("emptySet   | " + set());
        System.out.println("emptyMap   | " + map());
        // The rest of the immutable family, asked the same way. They share one
        // property — a mutator must throw `UnsupportedOperationException` — and
        // asking them together is what says whether a failure is one factory or
        // the whole family.
        mut("singletonList", () -> Collections.singletonList("a"));
        mut("singleton", () -> Collections.singleton("a"));
        mut("singletonMap", () -> Collections.singletonMap("a", "b"));
        mut("List.of", () -> List.of("a", "b"));
        mut("Set.of", () -> Set.of("a", "b"));
        mut("Map.of", () -> Map.of("a", "b"));
        mut("unmodifiableList", () -> Collections.unmodifiableList(new ArrayList<>(List.of("a"))));
        mut("unmodifiableSet", () -> Collections.unmodifiableSet(new HashSet<>(Set.of("a"))));
        mut("unmodifiableMap", () -> Collections.unmodifiableMap(new HashMap<String, String>()));
        mut("Arrays.asList", () -> Arrays.asList("a", "b"));
    }

    /** A mutator on an immutable view must raise, and must raise the SAME thing. */
    @SuppressWarnings({"unchecked", "rawtypes"})
    static void mut(String label, java.util.function.Supplier<Object> make) {
        Object o;
        try {
            o = make.get();
        } catch (Throwable t) {
            System.out.println(label + "  | BUILD ERROR " + t.getClass().getName());
            return;
        }
        String r;
        try {
            if (o instanceof Map) ((Map) o).put("k", "v");
            else ((Collection) o).add("v");
            r = "SUCCEEDED";
        } catch (UnsupportedOperationException e) {
            r = "UnsupportedOperationException";
        } catch (Throwable t) {
            r = t.getClass().getSimpleName();
        }
        System.out.println(label + "  | mutate=" + r + " class=" + o.getClass().getName());
    }

    static String list() {
        List<Object> a = Collections.emptyList();
        String cls = a.getClass().getName();
        String isAl = String.valueOf(a instanceof ArrayList);
        String add;
        try {
            a.add("phantom");
            add = "SUCCEEDED";
        } catch (UnsupportedOperationException e) {
            add = "UnsupportedOperationException";
        } catch (Throwable t) {
            add = t.getClass().getSimpleName();
        }
        // A DIFFERENT call, so a corrupted global is visible and a local copy is not.
        int seenByAnother = Collections.emptyList().size();
        return "class=" + cls + " instanceofArrayList=" + isAl + " add=" + add
                + " mutated=" + (seenByAnother != 0);
    }

    static String set() {
        Set<Object> a = Collections.emptySet();
        String cls = a.getClass().getName();
        String isHs = String.valueOf(a instanceof HashSet);
        String add;
        try {
            a.add("phantom");
            add = "SUCCEEDED";
        } catch (UnsupportedOperationException e) {
            add = "UnsupportedOperationException";
        } catch (Throwable t) {
            add = t.getClass().getSimpleName();
        }
        int seenByAnother = Collections.emptySet().size();
        return "class=" + cls + " instanceofHashSet=" + isHs + " add=" + add
                + " mutated=" + (seenByAnother != 0);
    }

    static String map() {
        Map<Object, Object> a = Collections.emptyMap();
        String cls = a.getClass().getName();
        String isHm = String.valueOf(a instanceof HashMap);
        String put;
        try {
            a.put("k", "phantom");
            put = "SUCCEEDED";
        } catch (UnsupportedOperationException e) {
            put = "UnsupportedOperationException";
        } catch (Throwable t) {
            put = t.getClass().getSimpleName();
        }
        int seenByAnother = Collections.emptyMap().size();
        return "class=" + cls + " instanceofHashMap=" + isHm + " put=" + put
                + " mutated=" + (seenByAnother != 0);
    }
}
