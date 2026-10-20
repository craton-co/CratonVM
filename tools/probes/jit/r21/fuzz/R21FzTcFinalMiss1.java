// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the compiled `instanceof` FINAL-class miss
// and its receiver-verdict screen (`CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT`,
// default ON, `=0` restores the old arm; `ir_lower`
// `typecheck_receiver_verdict_bytes`, single-pass `x64/op_object.rs`
// `final_miss_target`).
//
// Risk targeted: a definite `0` for `x instanceof F` (F final) is wrong for a
// receiver whose class the VM answers through a display-class rule (the
// `--compatible` `cratonvm/internal/Unmodifiable*` stamps against final
// `ImmutableCollections$ListN` & co.). Java-visible through the JDK's own
// `instanceof List12 / ListN / SetN / MapN` tests: `List.copyOf(x) == x`,
// `Set.copyOf`, `Map.copyOf`, `List.of(...).subList`, `Collections`
// wrappers. Also receivers the screen must keep answering correctly: lambda
// proxies, `java.lang.reflect.Proxy` instances, hidden classes, boxed values,
// arrays, null, against user final classes and JDK final classes.
//
// Run:
//   javac -d out R21FzTcFinalMiss1.java
//   java -Xmx256m -cp out R21FzTcFinalMiss1                               # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzTcFinalMiss1
//   cratonvm --compatible -Xmx256m -cp out R21FzTcFinalMiss1              # the stamp arm
//   CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT=0 cratonvm --compatible ... (kill: may show the old defect)
//   CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1 cratonvm ...; CRATONVM_C2_ACCEPT=never cratonvm ... (single-pass)
//   cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.function.Supplier;

public class R21FzTcFinalMiss1 {
    static final int WARM = 60_000;

    static final class Fin {
    }

    static final class Fin2 {
    }

    static boolean isFin(Object o) {
        return o instanceof Fin;
    }

    static boolean isString(Object o) {
        return o instanceof String;
    }

    static boolean isInteger(Object o) {
        return o instanceof Integer;
    }

    static boolean isLong(Object o) {
        return o instanceof Long;
    }

    static int bits(Object o) {
        return (isFin(o) ? 8 : 0) | (isString(o) ? 4 : 0) | (isInteger(o) ? 2 : 0) | (isLong(o) ? 1 : 0);
    }

    static <T> boolean listSame(List<T> l) {
        return List.copyOf(l) == l;
    }

    static <T> boolean setSame(Set<T> s) {
        return Set.copyOf(s) == s;
    }

    static <K, V> boolean mapSame(Map<K, V> m) {
        return Map.copyOf(m) == m;
    }

    static String identities() {
        List<Integer> l0 = List.of();
        List<Integer> l1 = List.of(1);
        List<Integer> l2 = List.of(1, 2);
        List<Integer> l3 = List.of(1, 2, 3);
        List<Integer> l10 = List.of(1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
        List<Integer> sub = List.of(1, 2, 3, 4).subList(1, 3);
        List<Integer> unm = Collections.unmodifiableList(new ArrayList<>(List.of(1, 2)));
        List<Integer> arr = new ArrayList<>(List.of(5));
        List<Integer> toList = java.util.stream.Stream.of(1, 2, 3).toList();
        Set<Integer> s0 = Set.of();
        Set<Integer> s1 = Set.of(1);
        Set<Integer> s3 = Set.of(1, 2, 3);
        Set<Integer> hs = new HashSet<>(Set.of(1, 2));
        Map<Integer, Integer> m0 = Map.of();
        Map<Integer, Integer> m1 = Map.of(1, 1);
        Map<Integer, Integer> m3 = Map.of(1, 1, 2, 2, 3, 3);
        Map<Integer, Integer> hm = new HashMap<>(Map.of(1, 1));
        StringBuilder sb = new StringBuilder();
        sb.append(listSame(l0)).append(listSame(l1)).append(listSame(l2)).append(listSame(l3))
                .append(listSame(l10)).append(listSame(sub)).append(listSame(unm)).append(listSame(arr))
                .append(listSame(toList)).append('/').append(setSame(s0)).append(setSame(s1))
                .append(setSame(s3)).append(setSame(hs)).append('/').append(mapSame(m0)).append(mapSame(m1))
                .append(mapSame(m3)).append(mapSame(hm));
        return sb.toString();
    }

    interface Marker {
        int m();
    }

    static Object[] receivers() {
        Supplier<String> lam = () -> "lam";
        Marker proxy = (Marker) Proxy.newProxyInstance(R21FzTcFinalMiss1.class.getClassLoader(),
                new Class<?>[] {Marker.class}, (p, m, a) -> 42);
        Runnable anon = new Runnable() {
            public void run() {
            }
        };
        return new Object[] {null, new Fin(), new Fin2(), "str", 3, 4L, lam, proxy, anon, new int[0],
            new Fin[0], List.of(1), Set.of(), Map.of(), new Object(), 'c', 1.5};
    }

    static void rows(String tag) {
        Object[] rs = receivers();
        StringBuilder sb = new StringBuilder("row " + tag + " bits");
        for (Object r : rs) {
            sb.append(' ').append(bits(r));
        }
        System.out.println(sb);
        System.out.println("row " + tag + " ident " + identities());
    }

    public static void main(String[] args) {
        rows("cold");
        Object[] rs = receivers();
        long h = 0;
        int same = 0;
        List<Integer> l3 = List.of(1, 2, 3);
        Set<Integer> s3 = Set.of(1, 2, 3);
        Map<Integer, Integer> m1 = Map.of(1, 1);
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + bits(rs[r % rs.length]);
            if (listSame(l3)) {
                same++;
            }
            if (setSame(s3)) {
                same++;
            }
            if (mapSame(m1)) {
                same++;
            }
        }
        System.out.println("warm hash " + h + " same=" + same);
        rows("hot");
        System.out.println("late " + bits(new Object() {
            @Override
            public String toString() {
                return "late";
            }
        }) + " " + listSame(Collections.nCopies(3, 1)));
        rows("after");
    }
}
