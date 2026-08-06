import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.NavigableSet;
import java.util.Set;
import java.util.SortedSet;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.function.Function;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * The collection views and factories that `--jdk-only` refuses to fabricate.
 *
 * L7 R1 made `--jdk-only` refuse to fabricate the `cratonvm/internal/*`
 * compatibility classes, which is right — contract §1.3 says real class bytes
 * are authoritative. Its plan was to retag the natives that MINT those classes
 * as `SyntheticStub`, so strict mode drops them and `java.base`'s own bytecode
 * runs. Where that retag has not happened, the native still mints a class the
 * VM now refuses, and ordinary JDK code dies with `NoClassDefFoundError`.
 *
 * Two such paths are already known, and neither mentions a collection:
 *
 *   Pattern.compile(",").split("1,2,3")     -> cratonvm/internal/ArrayListSubList
 *   Pattern.compile("b+").matcher("abbc").find(0); m.toMatchResult()
 *                                           -> cratonvm/internal/UnmodifiableMap
 *
 * This probe is the blast radius, not those two lines. Every operation is
 * public API, so the host JDK is the oracle: each must print the same thing
 * under `java`, `cratonvm --real-jdk` and `cratonvm --jdk-only`.
 *
 * Each case reports CONTENT, not just success — a view that comes back empty
 * because a real iterator read a field our natives never filled would otherwise
 * read as a pass. That failure mode is exactly why L7 R1 retagged the
 * unmodifiable family and left `HashSet.iterator()` alone.
 */
public final class JdkOnlyCollectionViewProbe {

    public static void main(String[] args) {
        subLists();
        unmodifiableViews();
        immutableFactories();
        comparators();
        functions();
        realWorldPaths();
        System.out.println("JdkOnlyCollectionViewProbe done");
    }

    static void subLists() {
        List<String> base = new ArrayList<>(Arrays.asList("a", "b", "c", "d"));
        p("sublist.mid", () -> join(base.subList(1, 3)));
        p("sublist.empty", () -> join(base.subList(2, 2)));
        p("sublist.size", () -> "" + base.subList(0, 3).size());
        p("sublist.contains", () -> "" + base.subList(1, 3).contains("c"));
        // A sub-list is a VIEW: writing through it must be visible in the base.
        p("sublist.set-writes-through", () -> {
            List<String> b2 = new ArrayList<>(Arrays.asList("a", "b", "c"));
            b2.subList(1, 2).set(0, "B");
            return join(b2);
        });
        p("sublist.of-sublist", () -> join(base.subList(0, 3).subList(1, 3)));
    }

    static void unmodifiableViews() {
        List<String> l = new ArrayList<>(Arrays.asList("x", "y"));
        p("unmod.list", () -> join(Collections.unmodifiableList(l)));
        p("unmod.list.throws", () -> {
            try {
                Collections.unmodifiableList(l).add("z");
                return "no-throw";
            } catch (UnsupportedOperationException e) {
                return "UnsupportedOperationException";
            }
        });
        Set<String> s = new java.util.LinkedHashSet<>(Arrays.asList("p", "q"));
        p("unmod.set", () -> join(Collections.unmodifiableSet(s)));
        Map<String, String> m = new LinkedHashMap<>();
        m.put("k1", "v1");
        m.put("k2", "v2");
        p("unmod.map", () -> mapStr(Collections.unmodifiableMap(m)));
        p("unmod.map.get", () -> Collections.unmodifiableMap(m).get("k2"));
        p("unmod.map.size", () -> "" + Collections.unmodifiableMap(m).size());
        p("unmod.collection", () -> join(Collections.unmodifiableCollection(l)));
        SortedSet<String> ss = new TreeSet<>(Arrays.asList("b", "a"));
        p("unmod.sortedset", () -> join(Collections.unmodifiableSortedSet(ss)));
        NavigableSet<String> ns = new TreeSet<>(Arrays.asList("d", "c"));
        p("unmod.navset", () -> join(Collections.unmodifiableNavigableSet(ns)));
        TreeMap<String, String> tm = new TreeMap<>();
        tm.put("z", "1");
        p("unmod.sortedmap", () -> mapStr(Collections.unmodifiableSortedMap(tm)));
        // The view must track the backing collection, not snapshot it.
        p("unmod.is-a-view", () -> {
            List<String> b = new ArrayList<>(Arrays.asList("1"));
            List<String> v = Collections.unmodifiableList(b);
            b.add("2");
            return join(v);
        });
    }

    static void immutableFactories() {
        p("List.of", () -> join(List.of("a", "b")));
        p("List.copyOf", () -> join(List.copyOf(new ArrayList<>(Arrays.asList("c", "d")))));
        p("Set.of", () -> "" + Set.of("e").size());
        p("Map.of", () -> mapStr(Map.of("mk", "mv")));
        p("Map.copyOf", () -> {
            Map<String, String> src = new LinkedHashMap<>();
            src.put("ck", "cv");
            return mapStr(Map.copyOf(src));
        });
        p("Map.entry", () -> {
            Map.Entry<String, String> e = Map.entry("ek", "ev");
            return e.getKey() + "=" + e.getValue();
        });
        p("Arrays.asList", () -> join(Arrays.asList("g", "h")));
        p("Collections.emptyList", () -> "" + Collections.emptyList().size());
        p("Collections.singletonList", () -> join(Collections.singletonList("s")));
    }

    static void comparators() {
        List<String> l = new ArrayList<>(Arrays.asList("bb", "a", "ccc"));
        p("comparator.naturalOrder", () -> {
            List<String> c = new ArrayList<>(l);
            c.sort(Comparator.naturalOrder());
            return join(c);
        });
        p("comparator.reverseOrder", () -> {
            List<String> c = new ArrayList<>(l);
            c.sort(Comparator.reverseOrder());
            return join(c);
        });
        p("comparator.comparing", () -> {
            List<String> c = new ArrayList<>(l);
            c.sort(Comparator.comparing(String::length));
            return join(c);
        });
        p("comparator.reversed", () -> {
            List<String> c = new ArrayList<>(l);
            c.sort(Comparator.<String>naturalOrder().reversed());
            return join(c);
        });
        p("comparator.thenComparing", () -> {
            List<String> c = new ArrayList<>(Arrays.asList("bb", "aa", "c"));
            c.sort(Comparator.comparing(String::length).thenComparing(Comparator.naturalOrder()));
            return join(c);
        });
    }

    static void functions() {
        p("Function.identity", () -> Function.identity().apply("id").toString());
        p("Function.andThen", () -> Function.<String>identity().andThen(x -> x + "!").apply("f"));
    }

    /** The two paths this was found through, neither of which names a collection. */
    static void realWorldPaths() {
        p("Pattern.split", () -> String.join("|", Pattern.compile(",").split("1,2,3")));
        p("Pattern.splitAsStream", () ->
                String.join("|", Pattern.compile(",").splitAsStream("4,5").toList()));
        p("Matcher.toMatchResult", () -> {
            Matcher m = Pattern.compile("b+").matcher("abbc");
            m.find(0);
            return m.toMatchResult().group() + "@" + m.toMatchResult().start();
        });
        p("String.split", () -> String.join("|", "6,7".split(",")));
    }

    interface T {
        String get() throws Exception;
    }

    static void p(String label, T t) {
        try {
            System.out.println(label + "=" + t.get());
        } catch (Throwable e) {
            String msg = e.getMessage();
            System.out.println(label + "=" + e.getClass().getName() + (msg == null ? "" : ": " + msg));
        }
    }

    static String join(java.util.Collection<?> c) {
        StringBuilder sb = new StringBuilder();
        for (Object o : c) {
            if (sb.length() > 0) {
                sb.append('|');
            }
            sb.append(o);
        }
        return "[" + sb + "]/" + c.size();
    }

    static String mapStr(Map<?, ?> m) {
        StringBuilder sb = new StringBuilder();
        for (Map.Entry<?, ?> e : m.entrySet()) {
            if (sb.length() > 0) {
                sb.append(',');
            }
            sb.append(e.getKey()).append('=').append(e.getValue());
        }
        return "{" + sb + "}/" + m.size();
    }
}
