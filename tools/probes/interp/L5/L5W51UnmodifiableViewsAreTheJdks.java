// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 51, lane L5 -- stage 4 of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020.md`:
// `Collections.unmodifiable*` hand out the JDK's own `Collections$Unmodifiable*`
// views (live views over the argument, refusing every mutator), and a
// `List.of` read back by serialization is the JDK's list. One row per
// factory: the view's class, a write through the backing collection seen
// through the view, and a mutator's exception. The first factory runs first
// on a cold VM and again last.
//
// `--compatible` before wave 51 (from the code): the views were
// `cratonvm/internal/Unmodifiable*` stand-ins (`alloc_unmod_wrapper`), or,
// for the sequenced views and any factory a later registrar's
// `native_return_first_arg` row won, the argument ITSELF (so `add` succeeded
// and the class was `java.util.ArrayList`).
//
// Run (no setup):
//   javac -d out L5W51UnmodifiableViewsAreTheJdks.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W51UnmodifiableViewsAreTheJdks
//
// Positive control: under `--compatible`,
// `CRATONVM_COMPAT_ALIGN=-jdk-unmodifiable-views` puts the stand-ins back (the
// `class=` column names `cratonvm.internal.*` or the backing class).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   list-1 class=java.util.Collections$UnmodifiableRandomAccessList live=[a, b] add=UnsupportedOperationException same=false
//   linked class=java.util.Collections$UnmodifiableList live=[a, b] add=UnsupportedOperationException same=false
//   collection class=java.util.Collections$UnmodifiableCollection live=[a, b] add=UnsupportedOperationException same=false
//   set class=java.util.Collections$UnmodifiableSet live=[a, b] add=UnsupportedOperationException same=false
//   sorted-set class=java.util.Collections$UnmodifiableSortedSet live=[a, b] add=UnsupportedOperationException same=false
//   navigable-set class=java.util.Collections$UnmodifiableNavigableSet live=[a, b] add=UnsupportedOperationException same=false
//   sequenced-collection class=java.util.Collections$UnmodifiableSequencedCollection live=[a, b] add=UnsupportedOperationException same=false
//   sequenced-set class=java.util.Collections$UnmodifiableSequencedSet live=[a, b] add=UnsupportedOperationException same=false
//   map class=java.util.Collections$UnmodifiableMap live={a=1, b=2} put=UnsupportedOperationException same=false
//   sorted-map class=java.util.Collections$UnmodifiableSortedMap live={a=1, b=2} put=UnsupportedOperationException same=false
//   navigable-map class=java.util.Collections$UnmodifiableNavigableMap live={a=1, b=2} put=UnsupportedOperationException same=false
//   sequenced-map class=java.util.Collections$UnmodifiableSequencedMap live={a=1, b=2} put=UnsupportedOperationException same=false
//   entry-set class=java.util.Collections$UnmodifiableMap$UnmodifiableEntrySet setValue=UnsupportedOperationException
//   rewrap list=true set=true map=true
//   serial class=java.util.ImmutableCollections$List12 equal=true
//   list-2 class=java.util.Collections$UnmodifiableRandomAccessList live=[a, b] add=UnsupportedOperationException same=false

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.function.Function;

public class L5W51UnmodifiableViewsAreTheJdks {
    static <C extends Collection<String>> void coll(String row, C backing,
            Function<C, Collection<String>> wrap) {
        Collection<String> view = wrap.apply(backing);
        backing.add("a");
        backing.add("b");
        String add;
        try {
            view.add("c");
            add = "ok";
        } catch (RuntimeException e) {
            add = e.getClass().getSimpleName();
        }
        System.out.println(row + " class=" + view.getClass().getName() + " live=" + view
                + " add=" + add + " same=" + (view == (Object) backing));
    }

    static <M extends Map<String, Integer>> void map(String row, M backing,
            Function<M, Map<String, Integer>> wrap) {
        Map<String, Integer> view = wrap.apply(backing);
        backing.put("a", 1);
        backing.put("b", 2);
        String put;
        try {
            view.put("c", 3);
            put = "ok";
        } catch (RuntimeException e) {
            put = e.getClass().getSimpleName();
        }
        System.out.println(row + " class=" + view.getClass().getName() + " live=" + view
                + " put=" + put + " same=" + (view == (Object) backing));
    }

    public static void main(String[] args) throws Exception {
        coll("list-1", new ArrayList<>(), Collections::unmodifiableList);
        coll("linked", new LinkedList<>(), Collections::unmodifiableList);
        coll("collection", new ArrayList<>(), Collections::unmodifiableCollection);
        coll("set", new LinkedHashSet<>(), Collections::unmodifiableSet);
        coll("sorted-set", new TreeSet<>(), Collections::unmodifiableSortedSet);
        coll("navigable-set", new TreeSet<>(), Collections::unmodifiableNavigableSet);
        coll("sequenced-collection", new ArrayList<>(),
                Collections::unmodifiableSequencedCollection);
        coll("sequenced-set", new LinkedHashSet<>(), Collections::unmodifiableSequencedSet);
        map("map", new LinkedHashMap<>(), Collections::unmodifiableMap);
        map("sorted-map", new TreeMap<>(), Collections::unmodifiableSortedMap);
        map("navigable-map", new TreeMap<>(), Collections::unmodifiableNavigableMap);
        map("sequenced-map", new LinkedHashMap<>(), Collections::unmodifiableSequencedMap);

        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("k", 1);
        Set<Map.Entry<String, Integer>> es = Collections.unmodifiableMap(m).entrySet();
        String setValue;
        try {
            es.iterator().next().setValue(9);
            setValue = "ok";
        } catch (RuntimeException e) {
            setValue = e.getClass().getSimpleName();
        }
        System.out.println("entry-set class=" + es.getClass().getName() + " setValue=" + setValue);

        // An unmodifiable view of an unmodifiable view is the same object
        // (`Collections.unmodifiableList` returns its argument when it is
        // already one of its classes).
        List<String> ul = Collections.unmodifiableList(new ArrayList<>());
        Set<String> us = Collections.unmodifiableSet(new LinkedHashSet<>());
        Map<String, Integer> um = Collections.unmodifiableMap(new LinkedHashMap<>());
        System.out.println("rewrap list=" + (Collections.unmodifiableList(ul) == ul)
                + " set=" + (Collections.unmodifiableSet(us) == us)
                + " map=" + (Collections.unmodifiableMap(um) == um));

        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(List.of("x", "y"));
        }
        Object back;
        try (ObjectInputStream in = new ObjectInputStream(
                new ByteArrayInputStream(bytes.toByteArray()))) {
            back = in.readObject();
        }
        System.out.println("serial class=" + back.getClass().getName()
                + " equal=" + back.equals(List.of("x", "y")));

        coll("list-2", new ArrayList<>(), Collections::unmodifiableList);
    }
}
