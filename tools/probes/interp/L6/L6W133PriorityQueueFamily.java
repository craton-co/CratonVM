// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 133, lane L6: the `java.util.PriorityQueue`
// surface the `jdk-priority-queue` HotSpot-alignment row moves onto the JDK's
// own bodies under `--compatible`
// (`i52-L5-proposal-a-compatible-row-per-retired-shadow-family`, "Wave 133";
// row default OFF). Every line must be HotSpot's in `--jdk-only`, and in
// `--compatible` with the row on; with it off, `--compatible` is the VM's
// stand-ins (a line that differs only there is what the row fixes: the
// stand-ins keep no `modCount`, so the fail-fast rows are the likely ones).
// Heap-array orders (`toString`, `toArray`, the iterator) are the JDK's
// siftUp / siftDown / heapify results, deterministic for these inputs.
// TIME lines are the bench (excluded from comparisons).
//
// Run: javac -d . L6W133PriorityQueueFamily.java
//      java -cp . L6W133PriorityQueueFamily                                     (HotSpot)
//      cratonvm --jdk-only -cp . L6W133PriorityQueueFamily
//      cratonvm --compatible -cp . L6W133PriorityQueueFamily
//      CRATONVM_COMPAT_JDK_PRIORITY_QUEUE=1 cratonvm --compatible -cp . L6W133PriorityQueueFamily
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// heap-natural [0, 1, 2, 4, 3, 8, 7, 5, 6, 9] size 10 peek 0
// to-array [0, 1, 2, 4, 3, 8, 7, 5, 6, 9]
// to-array-typed [0, 1, 2, 4, 3, 8, 7, 5, 6, 9]
// heap-reverse [9, 8, 7, 6, 3, 2, 5, 1, 4, 0] comparator-null false
// heap-capacity-2 [0, 1, 2, 4, 3, 8, 7, 5, 6, 9]
// heap-capacity-comparator [9, 8, 7, 6, 3, 2, 5, 1, 4, 0]
// drain-natural [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
// drain-reverse [9, 8, 7, 6, 5, 4, 3, 2, 1, 0]
// from-list [0, 1, 2, 4, 3, 8, 7, 5, 6, 9] comparator-null true
// from-sorted-as-collection [9, 8, 7, 6, 5, 4, 3, 2, 1, 0] drain [9, 8, 7, 6, 5, 4, 3, 2, 1, 0]
// from-sorted [9, 8, 7, 6, 5, 4, 3, 2, 1, 0]
// from-pq [9, 8, 7, 6, 3, 2, 5, 1, 4, 0] comparator-kept true
// from-pq-as-collection [9, 8, 7, 6, 3, 2, 5, 1, 4, 0]
// offer true add true [-1, 1, 0, 4, 3, 2, 7, 5, 6, 9, 11, 8]
// contains-7 true contains-42 false contains-null false
// remove-7 true remove-42 false remove-null false [-1, 1, 0, 4, 3, 2, 8, 5, 6, 9, 11]
// remove-head -1 element 0 [0, 1, 2, 4, 3, 11, 8, 5, 6, 9]
// duplicates [1, 1, 2, 3, 3]
// clear size 0 empty true peek null poll null str []
// remove-empty java.util.NoSuchElementException: null
// element-empty java.util.NoSuchElementException: null
// add-null java.lang.NullPointerException
// offer-null java.lang.NullPointerException
// capacity-0 java.lang.IllegalArgumentException: null
// from-null java.lang.NullPointerException
// from-list-with-null java.lang.NullPointerException
// non-comparable ClassCastException
// iterate 0 1 2 4 3 8 7 5 6 9
// iterator-class java.util.PriorityQueue$Itr
// iter-remove-evens [1, 3, 5, 7, 9] size 5 drain [1, 3, 5, 7, 9]
// next-past-end java.util.NoSuchElementException
// remove-before-next java.lang.IllegalStateException
// cme-add java.util.ConcurrentModificationException
// cme-poll java.util.ConcurrentModificationException
// cme-remove-object java.util.ConcurrentModificationException
// forget-me-not seen 0 10 3 1 11 12 2 left [0, 3, 1, 11, 12, 2]
// remove-if true [0, 1, 2, 4, 3, 5, 6]
// remove-all true [0, 4, 3, 5, 6]
// retain-all true [0, 3, 5, 6]
// add-all true [-3, 3, 0, 6, 9, 5, 4]
// for-each -3,3,0,6,9,5,4,
// stream-sorted -3,0,3,4,5,6,9
// stream-count 7 spliterator-size 7
// contains-all true
// add-all-self java.lang.IllegalArgumentException
// subclass offers 4 [1, 3, 2, 4] drain [1, 2, 3, 4]
// strings [fig, kiwi, date, pear, apple, banana] drain [fig, date, kiwi, pear, apple, banana]
// big n 1000 sum 505955 ordered true
// bench-acc 5079200
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Collections;
import java.util.Comparator;
import java.util.ConcurrentModificationException;
import java.util.Iterator;
import java.util.List;
import java.util.NoSuchElementException;
import java.util.PriorityQueue;
import java.util.TreeSet;
import java.util.stream.Collectors;

public class L6W133PriorityQueueFamily {
    static final Integer[] SEED = {5, 3, 8, 1, 9, 2, 7, 4, 6, 0};

    static PriorityQueue<Integer> filled(PriorityQueue<Integer> q) {
        for (Integer i : SEED) q.add(i);
        return q;
    }

    static String drain(PriorityQueue<?> q) {
        StringBuilder sb = new StringBuilder("[");
        Object o;
        boolean first = true;
        while ((o = q.poll()) != null) {
            if (!first) sb.append(", ");
            sb.append(o);
            first = false;
        }
        return sb.append(']').toString();
    }

    static String err(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static final class NotComparable {
        public String toString() { return "nc"; }
    }

    static final class CountingQueue<E> extends PriorityQueue<E> {
        int offers;
        @Override public boolean offer(E e) { offers++; return super.offer(e); }
    }

    public static void main(String[] args) {
        // Construction and heap layout.
        PriorityQueue<Integer> q = filled(new PriorityQueue<>());
        System.out.println("heap-natural " + q + " size " + q.size() + " peek " + q.peek());
        System.out.println("to-array " + Arrays.toString(q.toArray()));
        System.out.println("to-array-typed " + Arrays.toString(q.toArray(new Integer[0])));
        PriorityQueue<Integer> rev = filled(new PriorityQueue<>(Collections.reverseOrder()));
        System.out.println("heap-reverse " + rev + " comparator-null " + (rev.comparator() == null));
        PriorityQueue<Integer> cap = filled(new PriorityQueue<>(2));
        System.out.println("heap-capacity-2 " + cap);
        PriorityQueue<Integer> capCmp = filled(new PriorityQueue<>(3, Comparator.reverseOrder()));
        System.out.println("heap-capacity-comparator " + capCmp);
        System.out.println("drain-natural " + drain(filled(new PriorityQueue<>())));
        System.out.println("drain-reverse " + drain(rev));

        // The (Collection) constructor: heapify, and the runtime SortedSet /
        // PriorityQueue branches.
        List<Integer> list = new ArrayList<>(Arrays.asList(SEED));
        PriorityQueue<Integer> fromList = new PriorityQueue<>(list);
        System.out.println("from-list " + fromList + " comparator-null " + (fromList.comparator() == null));
        TreeSet<Integer> revSet = new TreeSet<>(Comparator.reverseOrder());
        revSet.addAll(list);
        PriorityQueue<Integer> fromSortedAsColl = new PriorityQueue<>((Collection<Integer>) revSet);
        System.out.println("from-sorted-as-collection " + fromSortedAsColl + " drain " + drain(fromSortedAsColl));
        PriorityQueue<Integer> fromSorted = new PriorityQueue<>(revSet);
        System.out.println("from-sorted " + fromSorted);
        PriorityQueue<Integer> fromPq = new PriorityQueue<>(filled(new PriorityQueue<>(Collections.reverseOrder())));
        System.out.println("from-pq " + fromPq + " comparator-kept " + (fromPq.comparator() != null));
        PriorityQueue<Integer> fromPqAsColl = new PriorityQueue<>((Collection<Integer>) filled(new PriorityQueue<>(Collections.reverseOrder())));
        System.out.println("from-pq-as-collection " + fromPqAsColl);

        // Element operations.
        q = filled(new PriorityQueue<>());
        System.out.println("offer " + q.offer(11) + " add " + q.add(-1) + " " + q);
        System.out.println("contains-7 " + q.contains(7) + " contains-42 " + q.contains(42) + " contains-null " + q.contains(null));
        System.out.println("remove-7 " + q.remove(7) + " remove-42 " + q.remove(42) + " remove-null " + q.remove(null) + " " + q);
        System.out.println("remove-head " + q.remove() + " element " + q.element() + " " + q);
        System.out.println("duplicates " + drain(new PriorityQueue<>(Arrays.asList(3, 1, 3, 1, 2))));
        q.clear();
        System.out.println("clear size " + q.size() + " empty " + q.isEmpty() + " peek " + q.peek() + " poll " + q.poll() + " str " + q);
        try { q.remove(); } catch (NoSuchElementException e) { System.out.println("remove-empty " + err(e)); }
        try { q.element(); } catch (NoSuchElementException e) { System.out.println("element-empty " + err(e)); }

        // Refusals.
        try { new PriorityQueue<Integer>().add(null); } catch (NullPointerException e) { System.out.println("add-null " + e.getClass().getName()); }
        try { new PriorityQueue<Integer>().offer(null); } catch (NullPointerException e) { System.out.println("offer-null " + e.getClass().getName()); }
        try { new PriorityQueue<Integer>(0); } catch (IllegalArgumentException e) { System.out.println("capacity-0 " + err(e)); }
        try { new PriorityQueue<Integer>((Collection<Integer>) null); } catch (NullPointerException e) { System.out.println("from-null " + e.getClass().getName()); }
        try { new PriorityQueue<Integer>(Arrays.asList(1, null, 2)); } catch (NullPointerException e) { System.out.println("from-list-with-null " + e.getClass().getName()); }
        try {
            PriorityQueue<Object> o = new PriorityQueue<>();
            o.add(new NotComparable());
            System.out.println("first-non-comparable size " + o.size());
            o.add(new NotComparable());
            System.out.println("second-non-comparable no exception");
        } catch (ClassCastException e) {
            System.out.println("non-comparable ClassCastException");
        }

        // Iteration: heap order, Iterator.remove, fail-fast.
        q = filled(new PriorityQueue<>());
        StringBuilder it = new StringBuilder();
        for (Integer i : q) it.append(i).append(' ');
        System.out.println("iterate " + it.toString().trim());
        Iterator<Integer> iter = q.iterator();
        System.out.println("iterator-class " + iter.getClass().getName());
        while (iter.hasNext()) if (iter.next() % 2 == 0) iter.remove();
        System.out.println("iter-remove-evens " + q + " size " + q.size() + " drain " + drain(new PriorityQueue<>(q)));
        try { iter.next(); } catch (NoSuchElementException e) { System.out.println("next-past-end " + e.getClass().getName()); }
        Iterator<Integer> iter2 = q.iterator();
        try { iter2.remove(); } catch (IllegalStateException e) { System.out.println("remove-before-next " + e.getClass().getName()); }
        try {
            for (Integer i : q) if (i == 3) q.add(100);
            System.out.println("cme-add none");
        } catch (ConcurrentModificationException e) {
            System.out.println("cme-add " + e.getClass().getName());
        }
        try {
            for (Integer i : q) if (i == 5) q.poll();
            System.out.println("cme-poll none");
        } catch (ConcurrentModificationException e) {
            System.out.println("cme-poll " + e.getClass().getName());
        }
        try {
            Iterator<Integer> a = q.iterator();
            a.next();
            q.remove(Integer.valueOf(100));
            a.next();
            System.out.println("cme-remove-object none");
        } catch (ConcurrentModificationException e) {
            System.out.println("cme-remove-object " + e.getClass().getName());
        }
        // The forgetMeNot path: removing an element whose replacement sifts up
        // past the cursor.
        PriorityQueue<Integer> f = new PriorityQueue<>(Arrays.asList(0, 10, 1, 11, 12, 2, 3));
        StringBuilder seen = new StringBuilder();
        for (Iterator<Integer> j = f.iterator(); j.hasNext(); ) {
            int v = j.next();
            seen.append(v).append(' ');
            if (v == 10) j.remove();
        }
        System.out.println("forget-me-not seen " + seen.toString().trim() + " left " + f);

        // Bulk operations the JDK inherits or overrides.
        q = filled(new PriorityQueue<>());
        System.out.println("remove-if " + q.removeIf(i -> i > 6) + " " + q);
        System.out.println("remove-all " + q.removeAll(Arrays.asList(1, 2)) + " " + q);
        System.out.println("retain-all " + q.retainAll(Arrays.asList(0, 3, 5, 6)) + " " + q);
        System.out.println("add-all " + q.addAll(Arrays.asList(9, -3, 4)) + " " + q);
        StringBuilder fe = new StringBuilder();
        q.forEach(i -> fe.append(i).append(','));
        System.out.println("for-each " + fe);
        System.out.println("stream-sorted " + q.stream().sorted().map(String::valueOf).collect(Collectors.joining(",")));
        System.out.println("stream-count " + q.stream().count() + " spliterator-size " + q.spliterator().estimateSize());
        System.out.println("contains-all " + q.containsAll(Arrays.asList(0, 9)));
        try { q.addAll(q); } catch (IllegalArgumentException e) { System.out.println("add-all-self " + e.getClass().getName()); }

        // A subclass: the JDK's add() calls offer().
        CountingQueue<Integer> cq = new CountingQueue<>();
        cq.add(3);
        cq.add(1);
        cq.addAll(Arrays.asList(2, 4));
        System.out.println("subclass offers " + cq.offers + " " + cq + " drain " + drain(cq));

        // Strings and a comparator over another key.
        PriorityQueue<String> s = new PriorityQueue<>(Comparator.comparing(String::length).thenComparing(Comparator.naturalOrder()));
        s.addAll(Arrays.asList("pear", "fig", "banana", "kiwi", "apple", "date"));
        System.out.println("strings " + s + " drain " + drain(s));

        // Large: a rehash of the backing array and a sorted drain.
        PriorityQueue<Integer> big = new PriorityQueue<>();
        int x = 12345;
        for (int i = 0; i < 1000; i++) { x = x * 1103515245 + 12345; big.add((x >>> 8) % 1000); }
        long sum = 0;
        int prev = Integer.MIN_VALUE;
        boolean ordered = true;
        int n = 0;
        while (!big.isEmpty()) { int v = big.poll(); if (v < prev) ordered = false; prev = v; sum += v; n++; }
        System.out.println("big n " + n + " sum " + sum + " ordered " + ordered);

        // Bench rows.
        long t0 = System.nanoTime();
        long acc = 0;
        for (int rep = 0; rep < 200; rep++) {
            PriorityQueue<Integer> b = new PriorityQueue<>();
            for (int i = 0; i < 200; i++) b.offer((i * 7919) % 211);
            while (!b.isEmpty()) acc += b.poll();
        }
        long t1 = System.nanoTime();
        PriorityQueue<Integer> it3 = new PriorityQueue<>(Arrays.asList(SEED));
        for (int rep = 0; rep < 20000; rep++) for (Integer i : it3) acc += i;
        long t2 = System.nanoTime();
        System.out.println("TIME offer-poll-200 " + (t1 - t0) / 200 + " ns/rep");
        System.out.println("TIME iterate-10 " + (t2 - t1) / 20000 + " ns/rep");
        System.out.println("bench-acc " + acc);
    }
}
