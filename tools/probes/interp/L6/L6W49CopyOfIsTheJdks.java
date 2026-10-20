// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6: `List.copyOf`, `Set.copyOf`,
// `Map.copyOf` and the `Collectors.toUnmodifiable*` finishers answer the JDK's
// own `ImmutableCollections` objects under `--compatible`, as on HotSpot and
// under `--jdk-only` (stage 4 of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020.md`,
// the `copyOf` producers). Until wave 49 `--compatible` answered three
// `SyntheticStub` natives with `cratonvm/internal/Unmodifiable*` stand-ins
// whose `getClass()` aliases to the JDK class: the positive controls are the
// `Unsafe` field reads, which a stand-in answers with its backing collection
// or `null` (`lc1 List12 e0=[x]`, `lcN ListN elements=not an array: [a, b,
// c]`, `mc1 Map1 k0=null`, ...), and `empty=false`. Only stdout is compared
// (HotSpot warns about `Unsafe` on stderr). The `lc1` row runs first (cold)
// and again last.
//
// HotSpot 25.0.3 prints (measured):
//     lc1 java.util.ImmutableCollections$List12 e0=x
//     lcN java.util.ImmutableCollections$ListN elements=[a, b, c]
//     sc1 java.util.ImmutableCollections$Set12 e0=x
//     scN java.util.ImmutableCollections$SetN elements=array size=3
//     mc1 java.util.ImmutableCollections$Map1 k0=k
//     mcN java.util.ImmutableCollections$MapN table=array size=3
//     tul java.util.ImmutableCollections$ListN elements=[a, b, c]
//     tus java.util.ImmutableCollections$SetN elements=array size=3
//     tum java.util.ImmutableCollections$MapN table=array size=2
//     identity=true true true
//     empty=true true true
//     snapshot=[x] [x] {k=v}
//     null=java.lang.NullPointerException java.lang.NullPointerException java.lang.NullPointerException
//     lc1-again java.util.ImmutableCollections$List12 e0=x
//
//     javac -d out L6W49CopyOfIsTheJdks.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W49CopyOfIsTheJdks
import java.lang.reflect.Field;
import java.util.*;
import java.util.concurrent.Callable;
import java.util.function.Function;
import java.util.stream.Collectors;
import java.util.stream.Stream;

public class L6W49CopyOfIsTheJdks {
    static sun.misc.Unsafe u;

    static String t(Callable<?> r) {
        try {
            return "ok " + r.call();
        } catch (Throwable e) {
            return e.getClass().getName();
        }
    }

    static String field(Object o, String name) throws Exception {
        Class<?> c = o.getClass();
        Object raw = u.getObject(o, u.objectFieldOffset(c.getDeclaredField(name)));
        if (raw instanceof Object[] a) {
            return name + "=" + (c.getSimpleName().equals("ListN") ? Arrays.toString(a) : "array size=" + sizeOf(o));
        }
        return name + "=" + raw;
    }

    static int sizeOf(Object o) {
        return o instanceof Map<?, ?> m ? m.size() : ((Collection<?>) o).size();
    }

    static void show(String label, Object o) throws Exception {
        String fieldName = switch (o.getClass().getSimpleName()) {
            case "List12", "Set12" -> "e0";
            case "Map1" -> "k0";
            case "MapN" -> "table";
            default -> "elements";
        };
        System.out.println(label + " " + o.getClass().getName() + " " + field(o, fieldName));
    }

    public static void main(String[] args) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        u = (sun.misc.Unsafe) f.get(null);

        List<String> src1 = new ArrayList<>(List.of("x"));
        List<String> lc1 = List.copyOf(src1);
        show("lc1", lc1);
        List<String> lcN = List.copyOf(new ArrayList<>(List.of("a", "b", "c")));
        show("lcN", lcN);
        Set<String> sc1 = Set.copyOf(new HashSet<>(Set.of("x")));
        show("sc1", sc1);
        Set<String> scN = Set.copyOf(new ArrayList<>(List.of("a", "b", "c", "a")));
        show("scN", scN);
        Map<String, String> mc1 = Map.copyOf(new HashMap<>(Map.of("k", "v")));
        show("mc1", mc1);
        Map<String, String> mcN = Map.copyOf(new HashMap<>(Map.of("a", "1", "b", "2", "c", "3")));
        show("mcN", mcN);
        show("tul", Stream.of("a", "b", "c").collect(Collectors.toUnmodifiableList()));
        show("tus", Stream.of("a", "b", "c").collect(Collectors.toUnmodifiableSet()));
        show("tum", Stream.of("a", "bb").collect(Collectors.toUnmodifiableMap(Function.identity(), String::length)));
        System.out.println("identity=" + (List.copyOf(lcN) == lcN) + " " + (Set.copyOf(scN) == scN) + " "
                + (Map.copyOf(mcN) == mcN));
        System.out.println("empty=" + (List.copyOf(new ArrayList<>()) == List.of()) + " "
                + (Set.copyOf(new ArrayList<>()) == Set.of()) + " " + (Map.copyOf(new HashMap<>()) == Map.of()));
        src1.add("y");
        Map<String, String> msrc = new HashMap<>(Map.of("k", "v"));
        Map<String, String> msnap = Map.copyOf(msrc);
        msrc.put("z", "w");
        System.out.println("snapshot=" + lc1 + " " + sc1 + " " + msnap);
        System.out.println("null=" + t(() -> List.copyOf(Arrays.asList("a", null))) + " "
                + t(() -> Set.copyOf(Arrays.asList("a", null))) + " "
                + t(() -> Map.copyOf(Collections.singletonMap("k", null))));
        show("lc1-again", List.copyOf(new ArrayList<>(List.of("x"))));
    }
}
