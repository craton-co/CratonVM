// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L6: `List.of(...)` under `--compatible`
// is the JDK's own `ImmutableCollections.List12` / `ListN`, as on HotSpot and
// under `--jdk-only`. Until wave 48 `--compatible` answered twelve
// `SyntheticStub` natives with a `cratonvm/internal/UnmodifiableList`
// stand-in whose `getClass()` aliased to the JDK class, but whose real class
// a debugger reported (`L1W23JdiConformance`, `this.tags`) and whose fields
// `Unsafe` read as the backing `ArrayList`: the positive controls are the
// `class=` rows, which `--compatible` printed as `l0 ... e0=[x]` and
// `l2 ... elements=not an array: [a, b, c, d]`, and `empty singleton=false`
// (measured on a wave-47-era build). The other rows are the plan's list surface
// (`docs/internal/fixed-bugs/interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020.md`).
// Only stdout is compared (HotSpot warns about `Unsafe` on stderr).
//
// HotSpot 25.0.3 prints (measured):
//     l0 class=java.util.ImmutableCollections$List12 e0=x
//     l0 size=1 isEmpty=false contains=true indexOf=-1 str=[x] hash=151 eq=true/true
//     l0 toArray=[x] stream=[X] iter=x forEach=x subList=[x] ra=true copyOf=true
//     l0 get0=ok x get9=java.lang.IndexOutOfBoundsException add=java.lang.UnsupportedOperationException
//     l0 ser=[x] java.util.ImmutableCollections$List12 true
//     l1 class=java.util.ImmutableCollections$List12 e0=x
//     l1 size=2 isEmpty=false contains=true indexOf=-1 str=[x, y] hash=4802 eq=true/true
//     l1 toArray=[x, y] stream=[X, Y] iter=xy forEach=xy subList=[x] ra=true copyOf=true
//     l1 get0=ok x get9=java.lang.IndexOutOfBoundsException add=java.lang.UnsupportedOperationException
//     l1 ser=[x, y] java.util.ImmutableCollections$List12 true
//     l2 class=java.util.ImmutableCollections$ListN elements=[a, b, c, d]
//     l2 size=4 isEmpty=false contains=false indexOf=1 str=[a, b, c, d] hash=3910595 eq=true/true
//     l2 toArray=[a, b, c, d] stream=[A, B, C, D] iter=abcd forEach=abcd subList=[a] ra=true copyOf=true
//     l2 get0=ok a get9=java.lang.ArrayIndexOutOfBoundsException add=java.lang.UnsupportedOperationException
//     l2 ser=[a, b, c, d] java.util.ImmutableCollections$ListN true
//     l3 class=java.util.ImmutableCollections$ListN elements=[]
//     l3 size=0 isEmpty=true contains=false indexOf=-1 str=[] hash=1 eq=true/true
//     l3 toArray=[] stream=[] iter= forEach= subList=- ra=true copyOf=true
//     l3 get0=java.lang.ArrayIndexOutOfBoundsException get9=java.lang.ArrayIndexOutOfBoundsException add=java.lang.UnsupportedOperationException
//     l3 ser=[] java.util.ImmutableCollections$ListN true
//     null element=java.lang.NullPointerException
//     empty singleton=true
//
//     javac -d out L6W48ListOfIsTheJdksList.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W48ListOfIsTheJdksList
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.RandomAccess;
import java.util.concurrent.Callable;

public class L6W48ListOfIsTheJdksList {
    static String t(Callable<?> r) {
        try {
            return "ok " + r.call();
        } catch (Throwable e) {
            return e.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) f.get(null);
        Class<?> list12 = Class.forName("java.util.ImmutableCollections$List12");
        Class<?> listN = Class.forName("java.util.ImmutableCollections$ListN");
        long e0 = u.objectFieldOffset(list12.getDeclaredField("e0"));
        long elements = u.objectFieldOffset(listN.getDeclaredField("elements"));

        List<List<String>> all = new ArrayList<>();
        all.add(List.of("x"));
        all.add(List.of("x", "y"));
        all.add(List.of("a", "b", "c", "d"));
        all.add(List.of());
        int i = 0;
        for (List<String> l : all) {
            String n = "l" + (i++);
            Object raw = u.getObject(l, l.getClass() == list12 ? e0 : elements);
            String field = l.getClass() == list12
                    ? "e0=" + raw
                    : "elements=" + (raw instanceof Object[] a ? Arrays.toString(a) : "not an array: " + raw);
            System.out.println(n + " class=" + l.getClass().getName() + " " + field);
            System.out.println(n + " size=" + l.size() + " isEmpty=" + l.isEmpty()
                    + " contains=" + l.contains("x") + " indexOf=" + l.indexOf("b")
                    + " str=" + l + " hash=" + l.hashCode()
                    + " eq=" + l.equals(new ArrayList<>(l)) + "/" + new ArrayList<>(l).equals(l));
            StringBuilder it = new StringBuilder();
            for (String s : l) {
                it.append(s);
            }
            StringBuilder fe = new StringBuilder();
            l.forEach(fe::append);
            System.out.println(n + " toArray=" + Arrays.toString(l.toArray())
                    + " stream=" + l.stream().map(String::toUpperCase).toList()
                    + " iter=" + it + " forEach=" + fe
                    + " subList=" + (l.isEmpty() ? "-" : l.subList(0, 1).toString())
                    + " ra=" + (l instanceof RandomAccess) + " copyOf=" + (List.copyOf(l) == l));
            System.out.println(n + " get0=" + t(() -> l.get(0)) + " get9=" + t(() -> l.get(9))
                    + " add=" + t(() -> l.add("z")));
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
                oo.writeObject(l);
            }
            Object back = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray())).readObject();
            System.out.println(n + " ser=" + back + " " + back.getClass().getName() + " " + back.equals(l));
        }
        System.out.println("null element=" + t(() -> List.of("a", null)));
        System.out.println("empty singleton=" + (List.of() == List.of()));
    }
}
