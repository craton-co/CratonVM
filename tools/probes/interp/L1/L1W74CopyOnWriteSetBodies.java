// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L1:
//
// * `CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET` (the `JdkCopyOnWriteArraySet`
//   HotSpot-alignment row, default OFF;
//   docs/internal/fixed-bugs/interpreter-i72-L1-proposal-a-copyonwritearrayset-family-row-20261105-DONE-20261107.md):
//   with it `--compatible` over a real JDK registers none of the 24
//   `HashSet`-surface natives `native-collections` mirrors onto
//   `CopyOnWriteArraySet`, so every call is the JDK 25 body with no native
//   door in front of it, as under `--jdk-only`.
//
// Row off, each of those natives sends a real set to its own bytecode
// (`cow_set_route`), so the expectation from code reading is SAME in all three
// runs; the rows below are the whole declared surface plus the inherited
// `hashCode` / `toString` / `stream` / `toArray(IntFunction)`, so a route that
// misses one shows up as a difference. The `interface` row reaches the set
// through `Collection` / `Set` / `Iterable` static types, where the
// `native-collections` interface natives (`register_interface_natives`; not
// this row's) can stand in front of the class.
//
// Run: `cratonvm L1W74CopyOnWriteSetBodies` (jdk-only), `cratonvm --compatible
// L1W74CopyOnWriteSetBodies` (row off, the default), and
// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET=1 cratonvm --compatible
// L1W74CopyOnWriteSetBodies` (row on); also with
// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE=1` beside it (the list row underneath).
//
// The expected block is DERIVED from the JDK 25 sources (CopyOnWriteArraySet,
// CopyOnWriteArrayList, Spliterators, AbstractSet), not yet captured from the
// host oracle: refresh it from the host's HotSpot run before reading a
// difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// empty: [] size=0 isEmpty=true
// add: b=true a=true b-again=false -> [b, a]
// contains: a=true z=false
// remove: z=false b=true -> [a]
// addAll: true -> [a, c, d] containsAll(a,d)=true containsAll(a,q)=false
// removeAll: true -> [a, c]
// retainAll: false -> [a, c]
// removeIf: true -> [a]
// iterator: java.util.concurrent.CopyOnWriteArrayList$COWIterator snapshot=3 live-size=4
// iterator.remove: java.lang.UnsupportedOperationException message=null
// toArray: [a, m, n, late] Object[]
// toArray(T[]): [a, m, n, late] String[]
// toArray(T[] big): [a, m, n, late, null, z] String[]
// toArray(IntFunction): [a, m, n, late] String[]
// stream: count(length==1)=3
// forEach: amnlate
// spliterator: estimate=4 distinct=true immutable=true sized=true ordered=false
// equals: toHashSet=true fromHashSet=true toList=false
// hashCode: matches-HashSet=true
// toString: [a, m, n, late]
// ctor(Collection): [x, y] fromSet=[x, y] class=CopyOnWriteArraySet
// deserialized: copy=[a, m, n, late, w] original=[a, m, n, late]
// subclass: SubSet [p] adds=2
// interface: Set.size=1 Collection.size=1 Collection.isEmpty=false Set.iterator=java.util.concurrent.CopyOnWriteArrayList$COWIterator Collection.iterator=java.util.concurrent.CopyOnWriteArrayList$COWIterator Iterable.iterator=java.util.concurrent.CopyOnWriteArrayList$COWIterator
// clear: [] isEmpty=true
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.Arrays;
import java.util.Collection;
import java.util.HashSet;
import java.util.Iterator;
import java.util.List;
import java.util.Set;
import java.util.Spliterator;
import java.util.concurrent.CopyOnWriteArraySet;

public class L1W74CopyOnWriteSetBodies {
    static final class SubSet extends CopyOnWriteArraySet<String> {
        private static final long serialVersionUID = 1L;
        int adds;

        @Override
        public boolean add(String e) {
            adds++;
            return super.add(e);
        }
    }

    interface Body {
        Object run() throws Exception;
    }

    static String exceptionOf(Body b) {
        try {
            return "no exception: " + b.run();
        } catch (Exception e) {
            return e.getClass().getName() + " message=" + e.getMessage();
        }
    }

    @SuppressWarnings("unchecked")
    static CopyOnWriteArraySet<String> roundTrip(CopyOnWriteArraySet<String> s) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(s);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return (CopyOnWriteArraySet<String>) in.readObject();
        }
    }

    public static void main(String[] args) throws Exception {
        CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
        System.out.println("empty: " + s + " size=" + s.size() + " isEmpty=" + s.isEmpty());

        boolean b1 = s.add("b");
        boolean a1 = s.add("a");
        boolean b2 = s.add("b");
        System.out.println("add: b=" + b1 + " a=" + a1 + " b-again=" + b2 + " -> " + s);
        System.out.println("contains: a=" + s.contains("a") + " z=" + s.contains("z"));
        boolean rz = s.remove("z");
        boolean rb = s.remove("b");
        System.out.println("remove: z=" + rz + " b=" + rb + " -> " + s);

        boolean aa = s.addAll(List.of("c", "a", "d"));
        System.out.println("addAll: " + aa + " -> " + s + " containsAll(a,d)=" + s.containsAll(List.of("a", "d"))
                + " containsAll(a,q)=" + s.containsAll(List.of("a", "q")));
        boolean ra = s.removeAll(List.of("d", "q"));
        System.out.println("removeAll: " + ra + " -> " + s);
        boolean ta = s.retainAll(List.of("a", "c", "x"));
        System.out.println("retainAll: " + ta + " -> " + s);
        boolean ri = s.removeIf("c"::equals);
        System.out.println("removeIf: " + ri + " -> " + s);

        s.addAll(List.of("m", "n"));
        Iterator<String> it = s.iterator();
        s.add("late");
        int seen = 0;
        while (it.hasNext()) {
            it.next();
            seen++;
        }
        System.out.println("iterator: " + it.getClass().getName() + " snapshot=" + seen + " live-size=" + s.size());
        System.out.println("iterator.remove: " + exceptionOf(() -> {
            Iterator<String> i = s.iterator();
            i.next();
            i.remove();
            return s;
        }));

        Object[] arr = s.toArray();
        System.out.println("toArray: " + Arrays.toString(arr) + " " + arr.getClass().getSimpleName());
        String[] typed = s.toArray(new String[0]);
        System.out.println("toArray(T[]): " + Arrays.toString(typed) + " " + typed.getClass().getSimpleName());
        String[] big = {"q", "q", "q", "q", "q", "z"};
        String[] into = s.toArray(big);
        System.out.println("toArray(T[] big): " + Arrays.toString(into) + " " + into.getClass().getSimpleName());
        String[] gen = s.toArray(String[]::new);
        System.out.println("toArray(IntFunction): " + Arrays.toString(gen) + " " + gen.getClass().getSimpleName());
        System.out.println("stream: count(length==1)=" + s.stream().filter(x -> x.length() == 1).count());
        StringBuilder sb = new StringBuilder();
        s.forEach(sb::append);
        System.out.println("forEach: " + sb);
        Spliterator<String> sp = s.spliterator();
        System.out.println("spliterator: estimate=" + sp.estimateSize()
                + " distinct=" + sp.hasCharacteristics(Spliterator.DISTINCT)
                + " immutable=" + sp.hasCharacteristics(Spliterator.IMMUTABLE)
                + " sized=" + sp.hasCharacteristics(Spliterator.SIZED)
                + " ordered=" + sp.hasCharacteristics(Spliterator.ORDERED));

        HashSet<String> hs = new HashSet<>(List.of("a", "m", "n", "late"));
        System.out.println("equals: toHashSet=" + s.equals(hs) + " fromHashSet=" + hs.equals(s)
                + " toList=" + s.equals(List.of("a", "m", "n", "late")));
        System.out.println("hashCode: matches-HashSet=" + (s.hashCode() == hs.hashCode()));
        System.out.println("toString: " + s.toString());

        CopyOnWriteArraySet<String> fromList = new CopyOnWriteArraySet<>(List.of("x", "y", "x"));
        CopyOnWriteArraySet<String> fromSet = new CopyOnWriteArraySet<>(fromList);
        System.out.println("ctor(Collection): " + fromList + " fromSet=" + fromSet
                + " class=" + fromSet.getClass().getSimpleName());

        CopyOnWriteArraySet<String> copy = roundTrip(s);
        copy.add("w");
        System.out.println("deserialized: copy=" + copy + " original=" + s);

        SubSet sub = new SubSet();
        sub.add("p");
        sub.add("p");
        System.out.println("subclass: " + sub.getClass().getSimpleName() + " " + sub + " adds=" + sub.adds);

        Set<String> asSet = new CopyOnWriteArraySet<>();
        asSet.add("q");
        Collection<String> asColl = asSet;
        Iterable<String> asIter = asSet;
        System.out.println("interface: Set.size=" + asSet.size() + " Collection.size=" + asColl.size()
                + " Collection.isEmpty=" + asColl.isEmpty()
                + " Set.iterator=" + asSet.iterator().getClass().getName()
                + " Collection.iterator=" + asColl.iterator().getClass().getName()
                + " Iterable.iterator=" + asIter.iterator().getClass().getName());

        s.clear();
        System.out.println("clear: " + s + " isEmpty=" + s.isEmpty());
        System.out.println("done");
    }
}
