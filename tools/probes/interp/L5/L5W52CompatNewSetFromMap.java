// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 52, lane L5 -- `Collections.newSetFromMap` under
// `--compatible`: the JDK returns a `Collections$SetFromMap` over the very map
// it was handed (`add(e)` is `map.put(e, TRUE)`, the map sees every element).
// The `--compatible` stand-in (`reflect_annotations.rs`
// `register_annotation_overrides`) returned a fresh `java.util.HashSet` over a
// NEW `HashMap` it wrote by slot for the synthetic layout -- over a real JDK
// that stored a bucket array in the real `HashMap.keySet` field and dropped
// two int stores (the exit `descriptor-coercion census` line) -- so the
// caller's map never saw an element. Rows, the case first and again last:
//
//   hash-N      a `HashMap` backing: the set's class, its contents, and the
//               backing map's contents after `add`.
//   linked-N    a `LinkedHashMap` backing (insertion order kept by the map).
//   remove-N    `remove` through the set empties the backing map.
//
// Run (no setup):
//   javac -d out L5W52CompatNewSetFromMap.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W52CompatNewSetFromMap
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   hash-1=java.util.Collections$SetFromMap [a] map={a=true}
//   linked-1=[z, a, m] map={z=true, a=true, m=true}
//   remove-1=true set=[] map={}
//   hash-2=java.util.Collections$SetFromMap [a] map={a=true}
//   linked-2=[z, a, m] map={z=true, a=true, m=true}
//   remove-2=true set=[] map={}

import java.util.Collections;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;

public class L5W52CompatNewSetFromMap {
    static void round(int r) {
        Map<String, Boolean> m = new HashMap<>();
        Set<String> s = Collections.newSetFromMap(m);
        s.add("a");
        System.out.println("hash-" + r + "=" + s.getClass().getName() + " " + s + " map=" + m);
        Map<String, Boolean> lm = new LinkedHashMap<>();
        Set<String> ls = Collections.newSetFromMap(lm);
        ls.add("z");
        ls.add("a");
        ls.add("m");
        System.out.println("linked-" + r + "=" + ls + " map=" + lm);
        Map<String, Boolean> rm = new HashMap<>();
        Set<String> rs = Collections.newSetFromMap(rm);
        rs.add("x");
        boolean removed = rs.remove("x");
        System.out.println("remove-" + r + "=" + removed + " set=" + rs + " map=" + rm);
    }

    public static void main(String[] args) {
        round(1);
        round(2);
    }
}
