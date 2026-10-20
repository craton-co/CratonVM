// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L1:
//
// * `CRATONVM_COMPAT_JDK_COPY_ON_WRITE` (the `JdkCopyOnWrite`
//   HotSpot-alignment row, default OFF;
//   docs/internal/fixed-bugs/interpreter-i71-L1-compatible-copyonwritearraylist-bridges-lock-the-list-not-its-lock-20261104-FIXED-20261107.md):
//   with it `--compatible` over a real JDK registers none of the
//   `CopyOnWriteArrayList` stand-ins (every one a retired-shadow row), so the
//   JDK 25 bodies run and every mutator locks the list's private `lock`, as
//   `--jdk-only` already did.
//
// Rows a stand-in run can differ on (expected from code reading): `mixed`
// (a stand-in `add` and the JDK's `addAll(int, ..)` hold different monitors,
// so the second `setArray` can drop the first's element: size short of
// 60000), `list-monitor` (a stand-in `add` waits on the LIST's monitor, which
// the main thread holds: `added=false` until it lets go), and the exception
// rows if a stand-in throws another type or message. `clone` and
// `deserialized` exercise `resetLock()`, the `lock` the 2026-07-14 bridges were
// added around. The two `interface iterator` rows are independent of the row:
// `native-collections` registers `Collection` / `List` / `Iterable.iterator`
// natives (`register_interface_natives`, `native_al_iterator`) that route a
// COWAL receiver to an `ArrayList$Itr` over a snapshot wrapper, whose
// `remove()` succeeds; the rows show whether an `invokeinterface` on a COWAL
// reaches them.
//
// Run: `cratonvm L1W72CopyOnWriteBodies` (jdk-only), `cratonvm --compatible
// L1W72CopyOnWriteBodies` (row off, the default: stand-ins), and
// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE=1 cratonvm --compatible L1W72CopyOnWriteBodies`
// (row on). jdk-only and the row-on arm must print the block below.
//
// The expected block is DERIVED from the JDK 25 sources
// (CopyOnWriteArrayList), not yet captured from the host oracle: refresh it
// from the host's HotSpot run before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// basic: [a, b, c] size=3 get(1)=b indexOf(c)=2 contains(z)=false
// mutate: set(0)=a add(1,x) addIfAbsent(b)=false addIfAbsent(d)=true remove(2)=b remove(c)=true -> [A, x, d]
// toArray: [A, x, d] length=3
// iterator: java.util.concurrent.CopyOnWriteArrayList$COWIterator snapshot=[A, x, d] live-size=4
// get(5): java.lang.ArrayIndexOutOfBoundsException message=Index 5 out of bounds for length 2
// add(9, z): java.lang.IndexOutOfBoundsException message=Index: 9, Size: 2
// remove(7): java.lang.ArrayIndexOutOfBoundsException message=Index 7 out of bounds for length 2
// iterator.remove: java.lang.UnsupportedOperationException message=null
// interface iterator: List=java.util.concurrent.CopyOnWriteArrayList$COWIterator Collection=java.util.concurrent.CopyOnWriteArrayList$COWIterator Iterable=java.util.concurrent.CopyOnWriteArrayList$COWIterator
// interface iterator.remove: java.lang.UnsupportedOperationException message=null list=[p, q]
// clear: [] isEmpty=true
// equals/hashCode: true true
// subclass: Sub [p, q] contains(q)=true
// clone: clone=[a, b, w] original=[a, b]
// deserialized: copy=[a, b, w] original=[a, b]
// list-monitor: added=true
// mixed: size=60000 x=20000 y=20000 z=20000
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Iterator;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;

public class L1W72CopyOnWriteBodies {
    static final int MIXED = 20_000;

    static final class Sub extends CopyOnWriteArrayList<String> {
        private static final long serialVersionUID = 1L;
    }

    interface Body {
        Object run() throws Exception;
    }

    static String exceptionOf(Body b) {
        try {
            return "no exception: " + b.run();
        } catch (Exception e) {
            return e.getClass().getName() + " message=" + e.getMessage();
        }
    }

    @SuppressWarnings("unchecked")
    static CopyOnWriteArrayList<String> roundTrip(CopyOnWriteArrayList<String> l) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(l);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return (CopyOnWriteArrayList<String>) in.readObject();
        }
    }

    public static void main(String[] args) throws Exception {
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>();
        l.add("a");
        l.add("b");
        l.add("c");
        System.out.println("basic: " + l + " size=" + l.size() + " get(1)=" + l.get(1)
                + " indexOf(c)=" + l.indexOf("c") + " contains(z)=" + l.contains("z"));

        String old = l.set(0, "A");
        l.add(1, "x");
        boolean absentB = l.addIfAbsent("b");
        boolean absentD = l.addIfAbsent("d");
        String removed = l.remove(2);
        boolean removedC = l.remove("c");
        System.out.println("mutate: set(0)=" + old + " add(1,x) addIfAbsent(b)=" + absentB
                + " addIfAbsent(d)=" + absentD + " remove(2)=" + removed + " remove(c)=" + removedC
                + " -> " + l);

        Object[] arr = l.toArray();
        System.out.println("toArray: " + Arrays.toString(arr) + " length=" + arr.length);

        Iterator<String> it = l.iterator();
        l.add("late");
        List<String> seen = new ArrayList<>();
        while (it.hasNext()) {
            seen.add(it.next());
        }
        System.out.println("iterator: " + it.getClass().getName() + " snapshot=" + seen
                + " live-size=" + l.size());

        CopyOnWriteArrayList<String> two = new CopyOnWriteArrayList<>(List.of("p", "q"));
        System.out.println("get(5): " + exceptionOf(() -> two.get(5)));
        System.out.println("add(9, z): " + exceptionOf(() -> {
            two.add(9, "z");
            return two;
        }));
        System.out.println("remove(7): " + exceptionOf(() -> two.remove(7)));
        System.out.println("iterator.remove: " + exceptionOf(() -> {
            Iterator<String> i = two.iterator();
            i.next();
            i.remove();
            return two;
        }));

        List<String> asList = new CopyOnWriteArrayList<>(List.of("p", "q"));
        Collection<String> asColl = asList;
        Iterable<String> asIter = asList;
        System.out.println("interface iterator: List=" + asList.iterator().getClass().getName()
                + " Collection=" + asColl.iterator().getClass().getName()
                + " Iterable=" + asIter.iterator().getClass().getName());
        System.out.println("interface iterator.remove: " + exceptionOf(() -> {
            Iterator<String> i = asList.iterator();
            i.next();
            i.remove();
            return asList;
        }) + " list=" + asList);

        l.clear();
        System.out.println("clear: " + l + " isEmpty=" + l.isEmpty());

        CopyOnWriteArrayList<String> e1 = new CopyOnWriteArrayList<>(List.of("p", "q"));
        System.out.println("equals/hashCode: " + e1.equals(List.of("p", "q"))
                + " " + (e1.hashCode() == List.of("p", "q").hashCode()));

        Sub sub = new Sub();
        sub.add("p");
        sub.add("q");
        System.out.println("subclass: " + sub.getClass().getSimpleName() + " " + sub
                + " contains(q)=" + sub.contains("q"));

        CopyOnWriteArrayList<String> orig = new CopyOnWriteArrayList<>(List.of("a", "b"));
        @SuppressWarnings("unchecked")
        CopyOnWriteArrayList<String> cl = (CopyOnWriteArrayList<String>) orig.clone();
        cl.add("w");
        System.out.println("clone: clone=" + cl + " original=" + orig);

        CopyOnWriteArrayList<String> copy = roundTrip(orig);
        copy.add("w");
        System.out.println("deserialized: copy=" + copy + " original=" + orig);

        CopyOnWriteArrayList<String> mon = new CopyOnWriteArrayList<>();
        boolean[] added = {false};
        Thread adder;
        synchronized (mon) {
            adder = new Thread(() -> {
                mon.add("m");
                added[0] = true;
            }, "list-monitor-adder");
            adder.setDaemon(true);
            adder.start();
            adder.join(5_000);
            System.out.println("list-monitor: added=" + added[0]);
        }
        adder.join(5_000);

        CopyOnWriteArrayList<String> mixed = new CopyOnWriteArrayList<>();
        Thread ta = new Thread(() -> {
            for (int i = 0; i < MIXED; i++) {
                mixed.add("x");
            }
        }, "mixed-add");
        Thread tb = new Thread(() -> {
            for (int i = 0; i < MIXED; i++) {
                mixed.addAll(0, List.of("y", "z"));
            }
        }, "mixed-addAll");
        ta.start();
        tb.start();
        ta.join();
        tb.join();
        int x = 0;
        int y = 0;
        int z = 0;
        for (String s : mixed) {
            switch (s) {
                case "x" -> x++;
                case "y" -> y++;
                default -> z++;
            }
        }
        System.out.println("mixed: size=" + mixed.size() + " x=" + x + " y=" + y + " z=" + z);
        System.out.println("done");
    }
}
