// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L6: `Set.of(...)` under `--compatible`
// is the JDK's own `ImmutableCollections.Set12` / `SetN`, as on HotSpot and
// under `--jdk-only` (stage 2 of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020.md`).
// Until wave 48 `--compatible` answered twelve `SyntheticStub` natives with a
// `cratonvm/internal/UnmodifiableSet` stand-in whose `getClass()` aliased to
// the JDK class. Positive control: the `field=` rows read the JDK class's own
// field through `Unsafe`, which the stand-in does not have; a wave-48 build
// before this stage printed `s0 ... field=[x]`, `s2 ... field=not an array:
// [a, b, c, d]` and `empty singleton=false` under `--compatible`. Element order of a `SetN` is salted
// per JVM, so every multi-element answer is sorted. Only stdout is compared.
//
// HotSpot 25.0.3 prints (measured):
//     s0 class=java.util.ImmutableCollections$Set12 field=x
//     s0 size=1 isEmpty=false contains=true/false sorted=[x] iter=[x] forEach=[x] hash=120
//     s0 eq=true/true toArray=[x] stream=[X] add=java.lang.UnsupportedOperationException copyOf=true
//     s0 ser=java.util.ImmutableCollections$Set12 true
//     s1 class=java.util.ImmutableCollections$Set12 field=x
//     s1 size=2 isEmpty=false contains=true/false sorted=[x, y] iter=[x, y] forEach=[x, y] hash=241
//     s1 eq=true/true toArray=[x, y] stream=[X, Y] add=java.lang.UnsupportedOperationException copyOf=true
//     s1 ser=java.util.ImmutableCollections$Set12 true
//     s2 class=java.util.ImmutableCollections$SetN field=[a, b, c, d]
//     s2 size=4 isEmpty=false contains=false/true sorted=[a, b, c, d] iter=[a, b, c, d] forEach=[a, b, c, d] hash=394
//     s2 eq=true/true toArray=[a, b, c, d] stream=[A, B, C, D] add=java.lang.UnsupportedOperationException copyOf=true
//     s2 ser=java.util.ImmutableCollections$SetN true
//     s3 class=java.util.ImmutableCollections$SetN field=[]
//     s3 size=0 isEmpty=true contains=false/false sorted=[] iter=[] forEach=[] hash=0
//     s3 eq=true/true toArray=[] stream=[] add=java.lang.UnsupportedOperationException copyOf=true
//     s3 ser=java.util.ImmutableCollections$SetN true
//     duplicate=java.lang.IllegalArgumentException: duplicate element: a
//     null element=java.lang.NullPointerException
//     empty singleton=true
//
//     javac -d out L6W48SetOfIsTheJdksSet.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W48SetOfIsTheJdksSet
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;
import java.util.concurrent.Callable;

public class L6W48SetOfIsTheJdksSet {
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
        Class<?> set12 = Class.forName("java.util.ImmutableCollections$Set12");
        Class<?> setN = Class.forName("java.util.ImmutableCollections$SetN");
        long e0 = u.objectFieldOffset(set12.getDeclaredField("e0"));
        long elements = u.objectFieldOffset(setN.getDeclaredField("elements"));

        List<Set<String>> all = new ArrayList<>();
        all.add(Set.of("x"));
        all.add(Set.of("x", "y"));
        all.add(Set.of("a", "b", "c", "d"));
        all.add(Set.of());
        int i = 0;
        for (Set<String> s : all) {
            String n = "s" + (i++);
            Object raw = u.getObject(s, s.getClass() == set12 ? e0 : elements);
            String field;
            if (s.getClass() == set12) {
                field = String.valueOf(raw);
            } else if (raw instanceof Object[] a) {
                TreeSet<String> sorted = new TreeSet<>();
                for (Object o : a) {
                    if (o != null) {
                        sorted.add(o.toString());
                    }
                }
                field = sorted.toString();
            } else {
                field = "not an array: " + raw;
            }
            System.out.println(n + " class=" + s.getClass().getName() + " field=" + field);
            TreeSet<String> it = new TreeSet<>();
            for (String e : s) {
                it.add(e);
            }
            TreeSet<String> fe = new TreeSet<>();
            s.forEach(fe::add);
            System.out.println(n + " size=" + s.size() + " isEmpty=" + s.isEmpty()
                    + " contains=" + s.contains("x") + "/" + s.contains("b")
                    + " sorted=" + new TreeSet<>(s) + " iter=" + it + " forEach=" + fe + " hash=" + s.hashCode());
            System.out.println(n + " eq=" + s.equals(new HashSet<>(s)) + "/" + new HashSet<>(s).equals(s)
                    + " toArray=" + new TreeSet<>(Arrays.asList(s.toArray()))
                    + " stream=" + new TreeSet<>(s.stream().map(String::toUpperCase).toList())
                    + " add=" + t(() -> s.add("z")) + " copyOf=" + (Set.copyOf(s) == s));
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
                oo.writeObject(s);
            }
            Object back = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray())).readObject();
            System.out.println(n + " ser=" + back.getClass().getName() + " " + back.equals(s));
        }
        System.out.println("duplicate=" + t(() -> Set.of("a", "a")));
        System.out.println("null element=" + t(() -> Set.of("a", null)));
        System.out.println("empty singleton=" + (Set.of() == Set.of()));
    }
}
