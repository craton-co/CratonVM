package cratonvm;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedList;
import java.util.TreeMap;
import java.util.Arrays;
import java.util.Collections;
import java.util.EnumSet;
import java.util.Optional;
import java.util.Iterator;
import java.util.ListIterator;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * TCK tests for java.util classes.
 *
 * Each test method returns 1 on success, 0 on failure.
 */
public class TckUtil {

    enum SmallEnumSetProbe { FIRST, SECOND }
    enum LargeEnumSetProbe { E0, E1, E2, E3, E4, E5, E6, E7, E8, E9, E10, E11, E12, E13, E14, E15, E16, E17, E18, E19, E20, E21, E22, E23, E24, E25, E26, E27, E28, E29, E30, E31, E32, E33, E34, E35, E36, E37, E38, E39, E40, E41, E42, E43, E44, E45, E46, E47, E48, E49, E50, E51, E52, E53, E54, E55, E56, E57, E58, E59, E60, E61, E62, E63, E64, E65, E66, E67, E68, E69 }

    // -----------------------------------------------------------------------
    // ArrayList
    // -----------------------------------------------------------------------

    /** Basic add, get, size, isEmpty, contains, indexOf. */
    public static int testArrayListBasic() {
        ArrayList list = new ArrayList();
        if (!list.isEmpty()) return 0;
        if (list.size() != 0) return 0;

        list.add("hello");
        list.add("world");
        if (list.size() != 2) return 0;
        if (list.isEmpty()) return 0;

        if (!"hello".equals(list.get(0))) return 0;
        if (!"world".equals(list.get(1))) return 0;

        if (!list.contains("hello")) return 0;
        if (list.contains("missing")) return 0;

        if (list.indexOf("world") != 1) return 0;
        if (list.indexOf("missing") != -1) return 0;

        return 1;
    }

    /** set, remove(index), remove(object), clear. */
    public static int testArrayListMutations() {
        ArrayList list = new ArrayList();
        list.add("a");
        list.add("b");
        list.add("c");

        // set
        Object old = list.set(1, "B");
        if (!"b".equals(old)) return 0;
        if (!"B".equals(list.get(1))) return 0;

        // remove by index
        Object removed = list.remove(0);
        if (!"a".equals(removed)) return 0;
        if (list.size() != 2) return 0;
        if (!"B".equals(list.get(0))) return 0;

        // remove by object
        list.add("d");
        boolean ok = list.remove("B");
        if (!ok) return 0;
        if (list.size() != 2) return 0;

        // clear
        list.clear();
        if (list.size() != 0) return 0;
        if (!list.isEmpty()) return 0;

        return 1;
    }

    /** Test ArrayList grows beyond initial capacity. */
    public static int testArrayListGrow() {
        ArrayList list = new ArrayList(2);
        for (int i = 0; i < 20; i++) {
            list.add(Integer.valueOf(i));
        }
        if (list.size() != 20) return 0;
        for (int i = 0; i < 20; i++) {
            Integer val = (Integer) list.get(i);
            if (val.intValue() != i) return 0;
        }
        return 1;
    }

    /** Test ArrayList iterator. */
    public static int testArrayListIterator() {
        ArrayList list = new ArrayList();
        list.add("x");
        list.add("y");
        list.add("z");

        Iterator it = list.iterator();
        int count = 0;
        while (it.hasNext()) {
            Object obj = it.next();
            count++;
        }
        if (count != 3) return 0;
        return 1;
    }

    /** Test ArrayList ListIterator reverse traversal. */
    public static int testArrayListListIteratorPrevious() {
        ArrayList list = new ArrayList();
        list.add("first");
        list.add("second");

        ListIterator it = list.listIterator(list.size());
        if (!it.hasPrevious()) return 0;
        if (!"second".equals(it.previous())) return 0;
        if (it.nextIndex() != 1) return 0;
        if (it.previousIndex() != 0) return 0;
        if (!it.hasPrevious()) return 0;
        if (!"first".equals(it.previous())) return 0;
        if (it.hasPrevious()) return 0;
        return 1;
    }

    /** Test add at index (insert). */
    public static int testArrayListInsert() {
        ArrayList list = new ArrayList();
        list.add("a");
        list.add("c");
        list.add(1, "b");
        if (list.size() != 3) return 0;
        if (!"a".equals(list.get(0))) return 0;
        if (!"b".equals(list.get(1))) return 0;
        if (!"c".equals(list.get(2))) return 0;
        return 1;
    }

    /** Test lastIndexOf. */
    public static int testArrayListLastIndexOf() {
        ArrayList list = new ArrayList();
        list.add("a");
        list.add("b");
        list.add("a");
        if (list.lastIndexOf("a") != 2) return 0;
        if (list.lastIndexOf("b") != 1) return 0;
        if (list.lastIndexOf("z") != -1) return 0;
        return 1;
    }

    // -----------------------------------------------------------------------
    // HashMap
    // -----------------------------------------------------------------------

    /** Basic put, get, containsKey, containsValue, size, isEmpty. */
    public static int testHashMapBasic() {
        HashMap map = new HashMap();
        if (!map.isEmpty()) return 0;
        if (map.size() != 0) return 0;

        map.put("key1", "val1");
        map.put("key2", "val2");
        if (map.size() != 2) return 0;
        if (map.isEmpty()) return 0;

        if (!"val1".equals(map.get("key1"))) return 0;
        if (!"val2".equals(map.get("key2"))) return 0;
        if (map.get("missing") != null) return 0;

        if (!map.containsKey("key1")) return 0;
        if (map.containsKey("missing")) return 0;

        if (!map.containsValue("val1")) return 0;
        if (map.containsValue("missing")) return 0;

        return 1;
    }

    /** Test put overwrites, remove, clear. */
    public static int testHashMapMutations() {
        HashMap map = new HashMap();
        map.put("k", "v1");
        Object old = map.put("k", "v2");
        if (!"v1".equals(old)) return 0;
        if (!"v2".equals(map.get("k"))) return 0;
        if (map.size() != 1) return 0;

        // remove
        Object removed = map.remove("k");
        if (!"v2".equals(removed)) return 0;
        if (map.size() != 0) return 0;

        // clear
        map.put("a", "1");
        map.put("b", "2");
        map.clear();
        if (map.size() != 0) return 0;
        if (!map.isEmpty()) return 0;

        return 1;
    }

    /** Test HashMap with Integer keys (tests hashCode/equals on boxed ints). */
    public static int testHashMapIntegerKeys() {
        HashMap map = new HashMap();
        for (int i = 0; i < 10; i++) {
            map.put(Integer.valueOf(i), Integer.valueOf(i * i));
        }
        if (map.size() != 10) return 0;
        for (int i = 0; i < 10; i++) {
            Integer val = (Integer) map.get(Integer.valueOf(i));
            if (val == null) return 0;
            if (val.intValue() != i * i) return 0;
        }
        return 1;
    }

    /** Test getOrDefault. */
    public static int testHashMapGetOrDefault() {
        HashMap map = new HashMap();
        map.put("exists", "yes");
        Object r1 = map.getOrDefault("exists", "no");
        if (!"yes".equals(r1)) return 0;
        Object r2 = map.getOrDefault("missing", "default");
        if (!"default".equals(r2)) return 0;
        return 1;
    }

    /** Test putIfAbsent. */
    public static int testHashMapPutIfAbsent() {
        HashMap map = new HashMap();
        map.put("k", "v1");
        Object r = map.putIfAbsent("k", "v2");
        if (!"v1".equals(r)) return 0;
        if (!"v1".equals(map.get("k"))) return 0;

        Object r2 = map.putIfAbsent("new", "val");
        if (r2 != null) return 0;
        if (!"val".equals(map.get("new"))) return 0;
        return 1;
    }

    // -----------------------------------------------------------------------
    // HashSet
    // -----------------------------------------------------------------------

    /** Basic add, contains, remove, size, isEmpty. */
    public static int testHashSetBasic() {
        HashSet set = new HashSet();
        if (!set.isEmpty()) return 0;
        if (set.size() != 0) return 0;

        boolean added1 = set.add("a");
        boolean added2 = set.add("b");
        boolean addedDup = set.add("a");
        if (!added1) return 0;
        if (!added2) return 0;
        if (addedDup) return 0;  // duplicate should return false
        if (set.size() != 2) return 0;

        if (!set.contains("a")) return 0;
        if (!set.contains("b")) return 0;
        if (set.contains("c")) return 0;

        boolean removed = set.remove("a");
        if (!removed) return 0;
        if (set.size() != 1) return 0;
        if (set.contains("a")) return 0;

        set.clear();
        if (!set.isEmpty()) return 0;
        return 1;
    }

    /** Test HashSet iterator. */
    public static int testHashSetIterator() {
        HashSet set = new HashSet();
        set.add("x");
        set.add("y");
        set.add("z");

        Iterator it = set.iterator();
        int count = 0;
        while (it.hasNext()) {
            it.next();
            count++;
        }
        if (count != 3) return 0;
        return 1;
    }

    // -----------------------------------------------------------------------
    // Arrays
    // -----------------------------------------------------------------------

    /** Test Arrays.sort on int[]. */
    public static int testArraysSort() {
        int[] arr = {5, 3, 1, 4, 2};
        Arrays.sort(arr);
        for (int i = 0; i < arr.length - 1; i++) {
            if (arr[i] > arr[i + 1]) return 0;
        }
        if (arr[0] != 1 || arr[4] != 5) return 0;
        return 1;
    }

    /** Test Arrays.copyOf. */
    public static int testArraysCopyOf() {
        Object[] src = {"a", "b", "c"};
        Object[] copy = Arrays.copyOf(src, 5);
        if (copy.length != 5) return 0;
        if (!"a".equals(copy[0])) return 0;
        if (!"b".equals(copy[1])) return 0;
        if (!"c".equals(copy[2])) return 0;
        if (copy[3] != null) return 0;
        if (copy[4] != null) return 0;

        // Shorter copy
        Object[] shorter = Arrays.copyOf(src, 2);
        if (shorter.length != 2) return 0;
        if (!"a".equals(shorter[0])) return 0;
        if (!"b".equals(shorter[1])) return 0;

        return 1;
    }

    /** Test Arrays.asList. */
    public static int testArraysAsList() {
        Object[] items = {"x", "y", "z"};
        java.util.List list = Arrays.asList(items);
        if (list.size() != 3) return 0;
        if (!"x".equals(list.get(0))) return 0;
        if (!"y".equals(list.get(1))) return 0;
        if (!"z".equals(list.get(2))) return 0;
        return 1;
    }

    // -----------------------------------------------------------------------
    // Collections utility
    // -----------------------------------------------------------------------

    /** Test Collections.emptyList. */
    public static int testCollectionsEmptyList() {
        java.util.List empty = Collections.emptyList();
        if (empty.size() != 0) return 0;
        if (!empty.isEmpty()) return 0;
        return 1;
    }

    /** Test Collections.singletonList. */
    public static int testCollectionsSingletonList() {
        java.util.List single = Collections.singletonList("only");
        if (single.size() != 1) return 0;
        if (!"only".equals(single.get(0))) return 0;
        return 1;
    }

    /** Test Collections.reverse. */
    public static int testCollectionsReverse() {
        ArrayList list = new ArrayList();
        list.add("a");
        list.add("b");
        list.add("c");
        Collections.reverse(list);
        if (!"c".equals(list.get(0))) return 0;
        if (!"b".equals(list.get(1))) return 0;
        if (!"a".equals(list.get(2))) return 0;
        return 1;
    }

    // -----------------------------------------------------------------------
    // Optional
    // -----------------------------------------------------------------------

    /** Test Optional.of, get, isPresent, isEmpty. */
    public static int testOptionalBasic() {
        Optional opt = Optional.of("hello");
        if (!opt.isPresent()) return 0;
        if (opt.isEmpty()) return 0;
        if (!"hello".equals(opt.get())) return 0;

        Optional empty = Optional.empty();
        if (empty.isPresent()) return 0;
        if (!empty.isEmpty()) return 0;

        return 1;
    }

    /** Test Optional.ofNullable and orElse. */
    public static int testOptionalOrElse() {
        Optional present = Optional.ofNullable("val");
        if (!"val".equals(present.orElse("default"))) return 0;

        Optional absent = Optional.ofNullable(null);
        if (!"default".equals(absent.orElse("default"))) return 0;

        return 1;
    }

    // -----------------------------------------------------------------------
    // Combined / integration tests
    // -----------------------------------------------------------------------

    /** Build a frequency map from a list of strings. */
    public static int testFrequencyMap() {
        ArrayList words = new ArrayList();
        words.add("apple");
        words.add("banana");
        words.add("apple");
        words.add("cherry");
        words.add("banana");
        words.add("apple");

        HashMap freq = new HashMap();
        Iterator it = words.iterator();
        while (it.hasNext()) {
            String w = (String) it.next();
            Integer count = (Integer) freq.get(w);
            if (count == null) {
                freq.put(w, Integer.valueOf(1));
            } else {
                freq.put(w, Integer.valueOf(count.intValue() + 1));
            }
        }

        if (freq.size() != 3) return 0;
        Integer appleCount = (Integer) freq.get("apple");
        Integer bananaCount = (Integer) freq.get("banana");
        Integer cherryCount = (Integer) freq.get("cherry");
        if (appleCount == null || appleCount.intValue() != 3) return 0;
        if (bananaCount == null || bananaCount.intValue() != 2) return 0;
        if (cherryCount == null || cherryCount.intValue() != 1) return 0;

        return 1;
    }

    /** Deduplicate a list using a HashSet. */
    public static int testDeduplication() {
        ArrayList list = new ArrayList();
        list.add("a");
        list.add("b");
        list.add("a");
        list.add("c");
        list.add("b");

        HashSet unique = new HashSet();
        Iterator it = list.iterator();
        while (it.hasNext()) {
            unique.add(it.next());
        }

        if (unique.size() != 3) return 0;
        if (!unique.contains("a")) return 0;
        if (!unique.contains("b")) return 0;
        if (!unique.contains("c")) return 0;

        return 1;
    }

    /** Test HashMap keySet iteration produces all keys. */
    public static int testHashMapKeySet() {
        HashMap map = new HashMap();
        map.put("k1", "v1");
        map.put("k2", "v2");
        map.put("k3", "v3");

        java.util.Set keys = map.keySet();
        if (keys.size() != 3) return 0;

        // Iterate keySet
        Iterator it = keys.iterator();
        int count = 0;
        while (it.hasNext()) {
            it.next();
            count++;
        }
        if (count != 3) return 0;

        return 1;
    }

    /** Test ArrayList with initial capacity constructor. */
    public static int testArrayListCapacity() {
        ArrayList list = new ArrayList(100);
        if (list.size() != 0) return 0;
        if (!list.isEmpty()) return 0;
        for (int i = 0; i < 50; i++) {
            list.add(Integer.valueOf(i));
        }
        if (list.size() != 50) return 0;
        return 1;
    }

    /** Test HashMap with initial capacity constructor. */
    public static int testHashMapCapacity() {
        HashMap map = new HashMap(64);
        if (map.size() != 0) return 0;
        for (int i = 0; i < 30; i++) {
            map.put(Integer.valueOf(i), Integer.valueOf(i * 10));
        }
        if (map.size() != 30) return 0;
        Integer v = (Integer) map.get(Integer.valueOf(15));
        if (v == null || v.intValue() != 150) return 0;
        return 1;
    }

    /** Test EnumSet.allOf iterator traversal. */
    public static int testEnumSetAllOfIterator() {
        int count = 0;
        for (SmallEnumSetProbe ignored : EnumSet.allOf(SmallEnumSetProbe.class)) {
            count++;
        }
        return count == 2 ? 1 : 0;
    }

    public static int testLargeEnumSetAllOfIterator() {
        int count = 0;
        for (LargeEnumSetProbe ignored : EnumSet.allOf(LargeEnumSetProbe.class)) {
            count++;
        }
        return count == 70 ? 1 : 0;
    }

    /** Test synchronizedCollection forEach delegates with an initialized mutex. */
    public static int testCollectionsSynchronizedCollectionForEach() {
        ArrayList list = new ArrayList();
        list.add(Integer.valueOf(1));
        list.add(Integer.valueOf(2));
        AtomicInteger sum = new AtomicInteger(0);
        Collections.synchronizedCollection(list).forEach(v -> sum.addAndGet(((Integer) v).intValue()));
        return sum.get() == 3 ? 1 : 0;
    }

    /** Test ArrayList toArray. */
    public static int testArrayListToArray() {
        ArrayList list = new ArrayList();
        list.add("p");
        list.add("q");
        list.add("r");
        Object[] arr = list.toArray();
        if (arr.length != 3) return 0;
        if (!"p".equals(arr[0])) return 0;
        if (!"q".equals(arr[1])) return 0;
        if (!"r".equals(arr[2])) return 0;
        return 1;
    }

    /** Test HashMap null key handling. */
    public static int testHashMapNullKey() {
        HashMap map = new HashMap();
        map.put(null, "nullval");
        map.put("key", "keyval");
        if (map.size() != 2) return 0;
        if (!"nullval".equals(map.get(null))) return 0;
        if (!map.containsKey(null)) return 0;
        Object removed = map.remove(null);
        if (!"nullval".equals(removed)) return 0;
        if (map.size() != 1) return 0;
        return 1;
    }

    // -----------------------------------------------------------------------
    // LinkedList
    // -----------------------------------------------------------------------

    /** LinkedList add/get/size/remove. */
    public static int linkedlist_basic() {
        LinkedList list = new LinkedList();
        list.add("first");
        list.add("second");
        list.add("third");
        if (list.size() != 3) return 0;
        if (!"first".equals(list.get(0))) return 0;
        if (!"second".equals(list.get(1))) return 0;
        if (!"third".equals(list.get(2))) return 0;

        // remove by index
        Object removed = list.remove(1);
        if (!"second".equals(removed)) return 0;
        if (list.size() != 2) return 0;
        if (!"third".equals(list.get(1))) return 0;

        // remove by object
        boolean ok = list.remove("first");
        if (!ok) return 0;
        if (list.size() != 1) return 0;
        if (!"third".equals(list.get(0))) return 0;

        // addFirst / addLast / getFirst / getLast
        list.addFirst("head");
        list.addLast("tail");
        if (!"head".equals(list.getFirst())) return 0;
        if (!"tail".equals(list.getLast())) return 0;
        if (list.size() != 3) return 0;

        return 1;
    }

    /** LinkedList.removeIf must route through a remove-capable iterator. */
    public static int linkedlist_remove_if_iterator_remove() {
        LinkedList list = new LinkedList();
        list.add("implemented");
        list.add("remaining");
        list.add("implemented");

        boolean changed = list.removeIf(new java.util.function.Predicate() {
            public boolean test(Object value) {
                return "implemented".equals(value);
            }
        });
        if (!changed) return 0;
        if (list.size() != 1) return 0;
        if (!"remaining".equals(list.getFirst())) return 0;

        Iterator it = list.iterator();
        if (!it.hasNext()) return 0;
        if (!"remaining".equals(it.next())) return 0;
        it.remove();
        if (list.size() != 0) return 0;

        return 1;
    }

    // -----------------------------------------------------------------------
    // TreeMap
    // -----------------------------------------------------------------------

    /** TreeMap put/get/firstKey/lastKey. */
    public static int treemap_basic() {
        TreeMap map = new TreeMap();
        map.put("cherry", Integer.valueOf(3));
        map.put("apple", Integer.valueOf(1));
        map.put("banana", Integer.valueOf(2));

        if (map.size() != 3) return 0;
        if (!Integer.valueOf(1).equals(map.get("apple"))) return 0;
        if (!Integer.valueOf(2).equals(map.get("banana"))) return 0;
        if (!Integer.valueOf(3).equals(map.get("cherry"))) return 0;

        // firstKey / lastKey (sorted order)
        if (!"apple".equals(map.firstKey())) return 0;
        if (!"cherry".equals(map.lastKey())) return 0;

        // containsKey
        if (!map.containsKey("banana")) return 0;
        if (map.containsKey("date")) return 0;

        // remove
        Object old = map.remove("banana");
        if (!Integer.valueOf(2).equals(old)) return 0;
        if (map.size() != 2) return 0;

        return 1;
    }

    // -----------------------------------------------------------------------
    // Arrays.sort (additional)
    // -----------------------------------------------------------------------

    /** Arrays.sort on int[] — named for conformance tier. */
    public static int arrays_sort() {
        int[] arr = {9, 3, 7, 1, 5, 2, 8, 4, 6};
        Arrays.sort(arr);
        for (int i = 0; i < arr.length - 1; i++) {
            if (arr[i] > arr[i + 1]) return 0;
        }
        if (arr[0] != 1 || arr[arr.length - 1] != 9) return 0;

        // Also test Arrays.sort on String[]
        String[] sarr = {"cherry", "apple", "banana"};
        Arrays.sort(sarr);
        if (!"apple".equals(sarr[0])) return 0;
        if (!"banana".equals(sarr[1])) return 0;
        if (!"cherry".equals(sarr[2])) return 0;

        return 1;
    }

    // -----------------------------------------------------------------------
    // Optional (additional)
    // -----------------------------------------------------------------------

    /** Optional.of/empty/isPresent/get — named for conformance tier. */
    public static int optional_basic() {
        Optional present = Optional.of("value");
        if (!present.isPresent()) return 0;
        if (!"value".equals(present.get())) return 0;

        Optional empty = Optional.empty();
        if (empty.isPresent()) return 0;

        // Test Optional.ofNullable with non-null
        Optional nullable = Optional.ofNullable("notnull");
        if (!nullable.isPresent()) return 0;
        if (!"notnull".equals(nullable.get())) return 0;

        // Test Optional.ofNullable with null
        Optional absent = Optional.ofNullable(null);
        if (absent.isPresent()) return 0;

        // Test orElse
        if (!"fallback".equals(absent.orElse("fallback"))) return 0;
        if (!"value".equals(present.orElse("fallback"))) return 0;

        return 1;
    }

    // -----------------------------------------------------------------------
    // Iterator (additional)
    // -----------------------------------------------------------------------

    /** Iterator over ArrayList — named for conformance tier. */
    public static int iterator_basic() {
        ArrayList list = new ArrayList();
        list.add("alpha");
        list.add("beta");
        list.add("gamma");

        Iterator it = list.iterator();
        if (!it.hasNext()) return 0;

        // Collect via iteration
        int count = 0;
        String first = null;
        String last = null;
        while (it.hasNext()) {
            String s = (String) it.next();
            if (count == 0) first = s;
            last = s;
            count++;
        }
        if (count != 3) return 0;
        if (!"alpha".equals(first)) return 0;
        if (!"gamma".equals(last)) return 0;
        if (it.hasNext()) return 0; // exhausted

        // Test iterator remove
        ArrayList list2 = new ArrayList();
        list2.add("a");
        list2.add("b");
        list2.add("c");
        Iterator it2 = list2.iterator();
        it2.next(); // "a"
        it2.remove();
        if (list2.size() != 2) return 0;
        if (!"b".equals(list2.get(0))) return 0;

        return 1;
    }
}
