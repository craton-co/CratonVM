// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round j27, wave 4, lane UNMODLIST -- CD16-4
// (`docs/feature-designs/collections-interception.md`).
//
// Every `Collections.unmodifiable*` view is LIVE on HotSpot: a write to the
// backing collection after the view, or one of its iterators, was made is seen
// through it, a structural write fails the view's iterator with the backing's
// own `ConcurrentModificationException`, and every mutator of the view, its
// iterators and its sub-views throws `UnsupportedOperationException`. The
// `cratonvm/internal/UnmodifiableList` stand-in's `listIterator([i])` walked a
// `toArray()` snapshot instead; the `jdk-unmodifiable-views` HotSpot-alignment
// row keeps that stand-in out of `--compatible` over a real JDK, and
// `--real-jdk` never had it.
//
// Run (no setup):
//   javac -d out J27UnmodViewRows.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out J27UnmodViewRows
// Every `fact` line must equal HotSpot 25's (`java -cp out J27UnmodViewRows`);
// the expected lines are listed on the page.
//
// Positive control: under `--compatible`,
// `CRATONVM_COMPAT_ALIGN=-jdk-unmodifiable-views` puts the stand-ins back, and
// at least `ral-listitr-sees-set` (`a`), `ral-listitr-idx-has-next-after`
// (`false`) and `ral-listitr-idx-next-after-add`
// (`java.util.NoSuchElementException:...`) then differ.

import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.ListIterator;
import java.util.Map;
import java.util.Set;
import java.util.SortedSet;
import java.util.Spliterator;
import java.util.TreeSet;
import java.util.function.Supplier;
import java.util.stream.Stream;

public class J27UnmodViewRows {
    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    /** The value, or `<exception class>:<message>`. */
    static String call(Supplier<Object> s) {
        try {
            return String.valueOf(s.get());
        } catch (RuntimeException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static String run(Runnable r) {
        return call(() -> {
            r.run();
            return "ok";
        });
    }

    static <L extends List<String>> L ab(L l) {
        l.add("a");
        l.add("b");
        return l;
    }

    /** `unmodifiableList` over `l` (`[a, b]`): list iterators, iterator, subList. */
    static void listRows(String p, List<String> l) {
        List<String> v = Collections.unmodifiableList(l);
        fact(p + "-view-class", v.getClass().getName());

        // A list iterator made before a NON-structural write sees the write.
        ListIterator<String> it = v.listIterator();
        fact(p + "-listitr-class", it.getClass().getName());
        l.set(0, "z");
        fact(p + "-listitr-sees-set", call(it::next));
        l.set(0, "a");

        // listIterator(i) at the end, then a STRUCTURAL write to the backing:
        // the live size answers hasNext, the backing's modCount fails next.
        ListIterator<String> end = v.listIterator(2);
        fact(p + "-listitr-idx-has-next-before", end.hasNext());
        l.add("c");
        fact(p + "-listitr-idx-has-next-after", end.hasNext());
        fact(p + "-listitr-idx-next-after-add", call(end::next));

        // previous() reads the live element too.
        ListIterator<String> back = v.listIterator(3);
        l.set(2, "y");
        fact(p + "-listitr-previous-sees-set", call(back::previous));

        // The iterator's mutators.
        ListIterator<String> ro = v.listIterator();
        ro.next();
        fact(p + "-listitr-remove", run(ro::remove));
        fact(p + "-listitr-set", run(() -> ro.set("q")));
        fact(p + "-listitr-add", run(() -> ro.add("q")));
        fact(p + "-listitr-idx-out-of-range", call(() -> v.listIterator(9)));

        // iterator(): the inherited UnmodifiableCollection one, live as well.
        Iterator<String> itr = v.iterator();
        fact(p + "-itr-class", itr.getClass().getName());
        l.add("d");
        fact(p + "-itr-next-after-add", call(itr::next));

        // subList of the view: another unmodifiable view, over the backing's
        // live subList.
        List<String> s = v.subList(0, 2);
        fact(p + "-sublist-class", s.getClass().getName());
        l.set(0, "z");
        fact(p + "-sublist-sees-set", call(() -> s.get(0)));
        fact(p + "-sublist-set", run(() -> s.set(0, "q")));
        fact(p + "-sublist-add", run(() -> s.add("q")));
        fact(p + "-sublist-listitr-class", s.listIterator().getClass().getName());
        fact(p + "-sublist-of-sublist-class", s.subList(0, 1).getClass().getName());
        l.add("e");
        fact(p + "-sublist-get-after-add", call(() -> s.get(0)));

        fact(p + "-view-live", v);
    }

    /** forEach / spliterator / stream of an `unmodifiableList(ArrayList)`. */
    static void bulkRows() {
        List<String> l = ab(new ArrayList<>());
        List<String> v = Collections.unmodifiableList(l);
        int[] seen = {0};
        fact("ral-for-each-add-during", run(() -> v.forEach(x -> {
            if (++seen[0] == 1) {
                l.add("c");
            }
        })));
        fact("ral-for-each-visited", seen[0]);

        List<String> l2 = ab(new ArrayList<>());
        List<String> v2 = Collections.unmodifiableList(l2);
        Spliterator<String> sp = v2.spliterator();
        l2.add("c");
        int[] n = {0};
        sp.forEachRemaining(x -> n[0]++);
        fact("ral-spliterator-late-binding", n[0]);
        Stream<String> st = v2.stream();
        l2.add("d");
        fact("ral-stream-late-binding", st.count());
    }

    static void collectionRows() {
        List<String> l = ab(new ArrayList<>());
        Collection<String> v = Collections.unmodifiableCollection(l);
        fact("coll-view-class", v.getClass().getName());
        Iterator<String> it = v.iterator();
        l.add("c");
        fact("coll-itr-next-after-add", call(it::next));
        fact("coll-view-live", v);
        fact("coll-itr-remove", run(() -> v.iterator().remove()));
        fact("coll-add", run(() -> v.add("q")));
    }

    static void setRows() {
        Set<String> s = new LinkedHashSet<>(List.of("a", "b"));
        Set<String> v = Collections.unmodifiableSet(s);
        fact("set-view-class", v.getClass().getName());
        Iterator<String> it = v.iterator();
        s.add("c");
        fact("set-itr-next-after-add", call(it::next));
        fact("set-view-live", v);
        fact("set-add", run(() -> v.add("q")));
        fact("set-remove", run(() -> v.remove("a")));
    }

    static void sortedSetRows() {
        TreeSet<String> ts = new TreeSet<>(List.of("a", "b"));
        SortedSet<String> v = Collections.unmodifiableSortedSet(ts);
        SortedSet<String> head = v.headSet("c");
        fact("sorted-head-class", head.getClass().getName());
        ts.add("bb");
        fact("sorted-head-live", head);
        Iterator<String> it = v.iterator();
        ts.add("c");
        fact("sorted-itr-next-after-add", call(it::next));
        fact("sorted-head-add", run(() -> head.add("a0")));
        fact("sorted-first", v.first());
    }

    static void mapRows() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        Map<String, Integer> v = Collections.unmodifiableMap(m);
        Set<String> ks = v.keySet();
        Collection<Integer> vs = v.values();
        Set<Map.Entry<String, Integer>> es = v.entrySet();
        fact("map-key-set-class", ks.getClass().getName());
        fact("map-values-class", vs.getClass().getName());
        fact("map-entry-set-class", es.getClass().getName());
        Iterator<Map.Entry<String, Integer>> eit = es.iterator();
        m.put("c", 3);
        fact("map-key-set-live", ks);
        fact("map-values-live", vs);
        fact("map-entry-itr-next-after-put", call(eit::next));
        fact("map-entry-set-value", run(() -> es.iterator().next().setValue(9)));
        fact("map-put", run(() -> v.put("q", 0)));
        fact("map-view-live", v);
    }

    public static void main(String[] args) {
        listRows("ral", ab(new ArrayList<>()));
        listRows("linked", ab(new LinkedList<>()));
        bulkRows();
        collectionRows();
        setRows();
        sortedSetRows();
        mapRows();
    }
}
