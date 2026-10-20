// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L5 -- which class each JDK collection
// factory answers, and the identity short-cuts the JDK takes on its own
// products. Under `--compatible` a registered stand-in can answer with
// CratonVM's own class (`cratonvm.internal.*`) or with a JDK class it builds
// differently (`docs/internal/fixed-bugs/interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020.md`:
// `Collections.emptyMap` / `emptySet` / `singleton*` / `synchronized*` are
// still `Bridge`s with their own products).
//
// Rows (one class name per factory, the first call and again last):
//   <factory>-N=<getClass().getName()>
//   rewrap-*  `Collections.unmodifiableX(view) == view` for a view the JDK
//             made, `List.copyOf(List.of(..)) == ...`
//   empty-same `Collections.emptyList() == Collections.emptyList()`
//
// Run (no setup):
//   javac -d out L5W55CompatCollectionClasses.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W55CompatCollectionClasses
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   emptyList-1=java.util.Collections$EmptyList
//   emptySet-1=java.util.Collections$EmptySet
//   emptyMap-1=java.util.Collections$EmptyMap
//   singletonList-1=java.util.Collections$SingletonList
//   singleton-1=java.util.Collections$SingletonSet
//   singletonMap-1=java.util.Collections$SingletonMap
//   synchronizedList-1=java.util.Collections$SynchronizedRandomAccessList
//   synchronizedMap-1=java.util.Collections$SynchronizedMap
//   synchronizedSet-1=java.util.Collections$SynchronizedSet
//   asList-1=java.util.Arrays$ArrayList
//   listOf0-1=java.util.ImmutableCollections$ListN
//   listOf1-1=java.util.ImmutableCollections$List12
//   listOf3-1=java.util.ImmutableCollections$ListN
//   setOf2-1=java.util.ImmutableCollections$Set12
//   mapOf1-1=java.util.ImmutableCollections$Map1
//   mapEntry-1=java.util.KeyValueHolder
//   toList-1=java.util.ImmutableCollections$ListN
//   collectToList-1=java.util.ArrayList
//   unmodList-1=java.util.Collections$UnmodifiableRandomAccessList
//   emptyIterator-1=java.util.Collections$EmptyIterator
//   nCopies-1=java.util.Collections$CopiesList
//   checkedList-1=java.util.Collections$CheckedRandomAccessList
//   reverseOrder-1=java.util.Collections$ReverseComparator
//   rewrap-copyOf=true
//   rewrap-set=true
//   empty-same=true
//   sync-contents=[x] size=1
//   singletonMap-entry=java.util.AbstractMap$SimpleImmutableEntry
//   emptyList-2=java.util.Collections$EmptyList
//   emptySet-2=java.util.Collections$EmptySet
//   emptyMap-2=java.util.Collections$EmptyMap
//   singletonList-2=java.util.Collections$SingletonList
//   singleton-2=java.util.Collections$SingletonSet
//   singletonMap-2=java.util.Collections$SingletonMap
//   synchronizedList-2=java.util.Collections$SynchronizedRandomAccessList
//   synchronizedMap-2=java.util.Collections$SynchronizedMap
//   synchronizedSet-2=java.util.Collections$SynchronizedSet
//   asList-2=java.util.Arrays$ArrayList
//   listOf0-2=java.util.ImmutableCollections$ListN
//   listOf1-2=java.util.ImmutableCollections$List12
//   listOf3-2=java.util.ImmutableCollections$ListN
//   setOf2-2=java.util.ImmutableCollections$Set12
//   mapOf1-2=java.util.ImmutableCollections$Map1
//   mapEntry-2=java.util.KeyValueHolder
//   toList-2=java.util.ImmutableCollections$ListN
//   collectToList-2=java.util.ArrayList
//   unmodList-2=java.util.Collections$UnmodifiableRandomAccessList
//   emptyIterator-2=java.util.Collections$EmptyIterator
//   nCopies-2=java.util.Collections$CopiesList
//   checkedList-2=java.util.Collections$CheckedRandomAccessList
//   reverseOrder-2=java.util.Collections$ReverseComparator
// CratonVM --compatible before the fix (debug build of 63d2087232, Windows):
// `nCopies-N=java.util.Collections$UnmodifiableRandomAccessList` (an `ArrayList` in
// the JDK's unmodifiable view): the `jdk-unmodifiable-views` row now drops
// `Collections.nCopies`'s three stand-ins as well, so the JDK body runs.

import java.util.*;
import java.util.stream.*;

public class L5W55CompatCollectionClasses {
    static String c(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static void classes(String tag) {
        System.out.println("emptyList-" + tag + "=" + c(Collections.emptyList()));
        System.out.println("emptySet-" + tag + "=" + c(Collections.emptySet()));
        System.out.println("emptyMap-" + tag + "=" + c(Collections.emptyMap()));
        System.out.println("singletonList-" + tag + "=" + c(Collections.singletonList("a")));
        System.out.println("singleton-" + tag + "=" + c(Collections.singleton("a")));
        System.out.println("singletonMap-" + tag + "=" + c(Collections.singletonMap("a", "b")));
        System.out.println("synchronizedList-" + tag + "=" + c(Collections.synchronizedList(new ArrayList<>())));
        System.out.println("synchronizedMap-" + tag + "=" + c(Collections.synchronizedMap(new HashMap<>())));
        System.out.println("synchronizedSet-" + tag + "=" + c(Collections.synchronizedSet(new HashSet<>())));
        System.out.println("asList-" + tag + "=" + c(Arrays.asList("a", "b")));
        System.out.println("listOf0-" + tag + "=" + c(List.of()));
        System.out.println("listOf1-" + tag + "=" + c(List.of("a")));
        System.out.println("listOf3-" + tag + "=" + c(List.of("a", "b", "c")));
        System.out.println("setOf2-" + tag + "=" + c(Set.of("a", "b")));
        System.out.println("mapOf1-" + tag + "=" + c(Map.of("a", "b")));
        System.out.println("mapEntry-" + tag + "=" + c(Map.entry("a", "b")));
        System.out.println("toList-" + tag + "=" + c(Stream.of("a", "b").toList()));
        System.out.println("collectToList-" + tag + "=" + c(Stream.of("a").collect(Collectors.toList())));
        System.out.println("unmodList-" + tag + "=" + c(Collections.unmodifiableList(new ArrayList<>())));
        System.out.println("emptyIterator-" + tag + "=" + c(Collections.emptyIterator()));
        System.out.println("nCopies-" + tag + "=" + c(Collections.nCopies(2, "a")));
        System.out.println("checkedList-" + tag + "=" + c(Collections.checkedList(new ArrayList<String>(), String.class)));
        System.out.println("reverseOrder-" + tag + "=" + c(Collections.reverseOrder()));
    }

    public static void main(String[] args) {
        classes("1");
        List<String> l = List.of("a", "b");
        System.out.println("rewrap-copyOf=" + (List.copyOf(l) == l));
        Set<String> s = Collections.unmodifiableSet(new HashSet<>());
        System.out.println("rewrap-set=" + (Collections.unmodifiableSet(s) == s));
        System.out.println("empty-same=" + (Collections.emptyList() == Collections.emptyList()));
        List<String> sync = Collections.synchronizedList(new ArrayList<>());
        sync.add("x");
        System.out.println("sync-contents=" + sync + " size=" + sync.size());
        Map<String, String> sm = Collections.singletonMap("k", "v");
        System.out.println("singletonMap-entry=" + c(sm.entrySet().iterator().next()));
        classes("2");
    }
}
