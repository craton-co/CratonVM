// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L6: `Map.of(...)` / `Map.ofEntries`
// under `--compatible` is the JDK's own `ImmutableCollections.Map1` / `MapN`,
// as on HotSpot and under `--jdk-only` (stage 3 of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020.md`).
// Until wave 48 `--compatible` answered twelve `SyntheticStub` natives with a
// `cratonvm/internal/UnmodifiableMap` stand-in whose `getClass()` aliased to
// the JDK class. Positive control: the `field=` rows read the JDK class's own
// field (`Map1.k0`, `MapN.size`) through `Unsafe`, which the stand-in does not
// have; a wave-48 build before this stage printed `m0 ... field=null`,
// `m1 ... field=0` and `empty singleton=false` under `--compatible`. `MapN`
// iteration order is salted per JVM, so multi-entry answers are
// sorted. Only stdout is compared.
//
// HotSpot 25.0.3 prints (measured):
//     m0 class=java.util.ImmutableCollections$Map1 field=k
//     m0 size=1 isEmpty=false has=false/false value=true get=null/null/d
//     m0 sorted={k=v} keys=[k] values=[v] entries={k=v} forEach={k=v} hash=29
//     m0 eq=true/true put=java.lang.UnsupportedOperationException copyOf=true putAll={k=v}
//     m0 ser=java.util.ImmutableCollections$Map1 true
//     m1 class=java.util.ImmutableCollections$MapN field=3
//     m1 size=3 isEmpty=false has=true/false value=true get=1/null/d
//     m1 sorted={a=1, b=2, c=3} keys=[a, b, c] values=[1, 2, 3] entries={a=1, b=2, c=3} forEach={a=1, b=2, c=3} hash=240
//     m1 eq=true/true put=java.lang.UnsupportedOperationException copyOf=true putAll={a=1, b=2, c=3}
//     m1 ser=java.util.ImmutableCollections$MapN true
//     m2 class=java.util.ImmutableCollections$MapN field=0
//     m2 size=0 isEmpty=true has=false/false value=false get=null/null/d
//     m2 sorted={} keys=[] values=[] entries={} forEach={} hash=0
//     m2 eq=true/true put=java.lang.UnsupportedOperationException copyOf=true putAll={}
//     m2 ser=java.util.ImmutableCollections$MapN true
//     m3 class=java.util.ImmutableCollections$MapN field=2
//     m3 size=2 isEmpty=false has=true/false value=true get=1/null/d
//     m3 sorted={a=1, b=2} keys=[a, b] values=[1, 2] entries={a=1, b=2} forEach={a=1, b=2} hash=160
//     m3 eq=true/true put=java.lang.UnsupportedOperationException copyOf=true putAll={a=1, b=2}
//     m3 ser=java.util.ImmutableCollections$MapN true
//     duplicate=java.lang.IllegalArgumentException: duplicate key: a
//     null key=java.lang.NullPointerException
//     empty singleton=true
//
//     javac -d out L6W48MapOfIsTheJdksMap.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W48MapOfIsTheJdksMap
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.Callable;

public class L6W48MapOfIsTheJdksMap {
    static String t(Callable<?> r) {
        try {
            return "ok " + r.call();
        } catch (Throwable e) {
            String m = e.getMessage();
            return e.getClass().getName() + (m == null ? "" : ": " + m);
        }
    }

    public static void main(String[] args) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) f.get(null);
        Class<?> map1 = Class.forName("java.util.ImmutableCollections$Map1");
        Class<?> mapN = Class.forName("java.util.ImmutableCollections$MapN");
        long k0 = u.objectFieldOffset(map1.getDeclaredField("k0"));
        long size = u.objectFieldOffset(mapN.getDeclaredField("size"));

        List<Map<String, String>> all = new ArrayList<>();
        all.add(Map.of("k", "v"));
        all.add(Map.of("a", "1", "b", "2", "c", "3"));
        all.add(Map.of());
        all.add(Map.ofEntries(Map.entry("a", "1"), Map.entry("b", "2")));
        int i = 0;
        for (Map<String, String> m : all) {
            String n = "m" + (i++);
            String field = m.getClass() == map1
                    ? String.valueOf(u.getObject(m, k0))
                    : String.valueOf(u.getInt(m, size));
            System.out.println(n + " class=" + m.getClass().getName() + " field=" + field);
            System.out.println(n + " size=" + m.size() + " isEmpty=" + m.isEmpty()
                    + " has=" + m.containsKey("a") + "/" + m.containsKey("zz") + " value=" + m.containsValue(m.isEmpty() ? "?" : m.values().iterator().next())
                    + " get=" + m.get("a") + "/" + m.get("zz") + "/" + m.getOrDefault("zz", "d"));
            TreeMap<String, String> es = new TreeMap<>();
            for (Map.Entry<String, String> e : m.entrySet()) {
                es.put(e.getKey(), e.getValue());
            }
            TreeMap<String, String> fe = new TreeMap<>();
            m.forEach(fe::put);
            System.out.println(n + " sorted=" + new TreeMap<>(m) + " keys=" + new TreeSet<>(m.keySet())
                    + " values=" + new TreeSet<>(m.values()) + " entries=" + es + " forEach=" + fe + " hash=" + m.hashCode());
            HashMap<String, String> h = new HashMap<>();
            h.putAll(m);
            System.out.println(n + " eq=" + m.equals(new HashMap<>(m)) + "/" + new HashMap<>(m).equals(m)
                    + " put=" + t(() -> m.put("z", "z")) + " copyOf=" + (Map.copyOf(m) == m) + " putAll=" + new TreeMap<>(h));
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
                oo.writeObject(m);
            }
            Object back = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray())).readObject();
            System.out.println(n + " ser=" + back.getClass().getName() + " " + back.equals(m));
        }
        System.out.println("duplicate=" + t(() -> Map.of("a", "1", "a", "2")));
        System.out.println("null key=" + t(() -> Map.of("a", "1", null, "2")));
        System.out.println("empty singleton=" + (Map.of() == Map.of()));
    }
}
