// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L1: the shapes of the 2026-07-14
// `Cannot enter synchronized block because "this.lock" is null` NPE, for
// deciding the `JdkCopyOnWrite` flip (`CRATONVM_COMPAT_JDK_COPY_ON_WRITE`)
// without the Spring Boot sample
// (docs/internal/fixed-bugs/interpreter-i73-L1-proposal-decide-jdk-copy-on-write-without-the-spring-sample-20261106-DONE-20261107.md).
//
// The original record (commit 68c44f62; docs/internal/fixed-suite-bugs/spring/
// CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md, "2026-07-14 update #2"):
// Spring's `CrossOriginAnnotationIntegrationTests` failed every sub-test once
// the `util_concurrent_ext` COWAL mutator stand-ins (`add`, `set`, `remove` x2,
// `clear`) were dropped as mis-tagged stubs, so the JDK's `synchronized (lock)`
// bodies ran on lists whose `lock` slot did not hold the lock. The JDK
// constructor ran (`<init>` was never a stand-in), so `lock` had been set;
// what broke it was a STAND-IN that stayed: `native-collections`'
// `addIfAbsent` / `addAll(Collection)` / `bulkRemove` bumped
// `AbstractList.modCount` through `resolve_field_index("java/util/AbstractList",
// "modCount")`, which is slot 0 -- COWAL's `lock` (COWAL does not extend
// `AbstractList`), storing an `int` over the monitor object. Spring's
// `addBeanPostProcessors` is exactly `removeAll(c)` (JDK body, then the
// `bulkRemove` stand-in) + `addAll(c)` (stand-in: clobbers `lock`), and the
// next `addBeanPostProcessor` is `remove(o)` + `add(o)` (JDK bodies once the
// stand-ins were dropped): NPE. The same corruption was measured again on
// 2026-08-12 (`addAllAbsent` NPE after `addIfAbsent`) and fixed in
// `cowal_bump_mod_count` (ask the receiver's class; never write the two COW
// slots). With the row ON none of those stand-ins is registered, so the root
// cause cannot recur through them; the rows below drive every stand-in
// (row off) and then a JDK body that reads `lock`, plus every other way a
// COWAL comes to exist: each constructor, reflective and method-handle
// construction, `clone`, deserialization (both `resetLock()`), a subclass,
// a `CopyOnWriteArraySet`'s list, `subList`, JDK-internal lists
// (`java.util.logging.Logger` handlers), and calls through `List` /
// `Collection` static types. `unsafe-allocated` is the one shape with a null
// `lock` on HotSpot too (no constructor ran): the JDK body must throw there.
//
// `jdk(...)` after each row is a battery of JDK bodies that all read `lock`
// (`addAllAbsent`, `removeAll`, `retainAll`, `replaceAll`, `subList`, `sort`);
// a corrupted or missing `lock` prints an exception instead of the list.
//
// Run: `cratonvm L1W74CopyOnWriteLockShapes` (jdk-only), `cratonvm --compatible
// L1W74CopyOnWriteLockShapes` (row off, the default: stand-ins), and
// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE=1 cratonvm --compatible
// L1W74CopyOnWriteLockShapes` (row on). jdk-only and the row-on arm must print
// the block below; row off, `list-monitor` is the expected difference (the
// `util_concurrent_ext` mutators lock the list, not `lock`) and
// `unsafe-allocated` may differ (the stand-ins repair a null `lock`; in any
// mode, `mentions-lock` also needs the helpful-NPE text for `monitorenter`).
//
// The expected block is DERIVED from the JDK 25 sources, not yet captured
// from the host oracle: refresh it from the host's HotSpot run before reading
// a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// spring-bpp-list: [a, b, c, d] jdk=[D, C, A, E, F] resets=10 cache=null
// after-addIfAbsent: jdk=[a, b, c, k]
// after-addAll: jdk=[a, b, k]
// after-removeIf: jdk=[a, b, k]
// after-readers: contains=true indexOf=1 get=a size=2 isEmpty=false next=b toArray=2 jdk=[a, b, k]
// after-mutators: [a, b] removed(q)=false jdk=[a, b, k]
// after-clear: jdk=[b, k]
// ctor(): jdk=[k]
// ctor(ArrayList): jdk=[a, b, k]
// ctor(COWAL): jdk=[a, b, k] source=[b, a]
// ctor(Object[]): jdk=[a, b, k]
// reflect ctor(): jdk=[b, k]
// reflect ctor(Collection): jdk=[a, b, k]
// method-handle ctor(): jdk=[b, k]
// subclass clone: jdk=[a, b, c, k] original=[b, a] class=Sub
// subclass deserialized: jdk=[a, b, c, k] class=Sub
// list deserialized in a holder: jdk=[a, b, c, k]
// interface: [a, b, c] size=3 get(1)=b jdk=[a, b, c, k]
// cowas: [b, a] add(b)=false
// subList: [a, x, d]
// jul-handlers: added=1 removed=0
// unsafe-allocated: add=java.lang.NullPointerException mentions-lock=true
// list-monitor: add=true add(int)=true set=true remove(int)=true remove(Object)=true addIfAbsent=true addAll=true holds=true
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Collection;
import java.util.Comparator;
import java.util.Iterator;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CopyOnWriteArraySet;
import java.util.function.Predicate;
import java.util.function.UnaryOperator;
import java.util.logging.Handler;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

public class L1W74CopyOnWriteLockShapes {
    /**
     * Spring 6's `AbstractBeanFactory`: an inner (non-static) COWAL subclass
     * whose mutators reset a cache on the outer object, and the two add
     * methods that `synchronized (list)` around `remove`/`removeAll` + `add`/`addAll`.
     */
    static final class Factory {
        Object cache = "warm";
        int resets;
        final List<String> beanPostProcessors = new CacheAwareList();

        private final class CacheAwareList extends CopyOnWriteArrayList<String> {
            private static final long serialVersionUID = 1L;

            private void reset() {
                cache = null;
                resets++;
            }

            @Override
            public String set(int index, String element) {
                String r = super.set(index, element);
                reset();
                return r;
            }

            @Override
            public boolean add(String e) {
                boolean r = super.add(e);
                reset();
                return r;
            }

            @Override
            public void add(int index, String element) {
                super.add(index, element);
                reset();
            }

            @Override
            public String remove(int index) {
                String r = super.remove(index);
                reset();
                return r;
            }

            @Override
            public boolean remove(Object o) {
                boolean r = super.remove(o);
                reset();
                return r;
            }

            @Override
            public boolean removeAll(Collection<?> c) {
                boolean r = super.removeAll(c);
                reset();
                return r;
            }

            @Override
            public boolean retainAll(Collection<?> c) {
                boolean r = super.retainAll(c);
                reset();
                return r;
            }

            @Override
            public boolean addAll(Collection<? extends String> c) {
                boolean r = super.addAll(c);
                reset();
                return r;
            }

            @Override
            public boolean addAll(int index, Collection<? extends String> c) {
                boolean r = super.addAll(index, c);
                reset();
                return r;
            }

            @Override
            public boolean removeIf(Predicate<? super String> filter) {
                boolean r = super.removeIf(filter);
                reset();
                return r;
            }

            @Override
            public void replaceAll(UnaryOperator<String> operator) {
                super.replaceAll(operator);
                reset();
            }
        }

        void addBeanPostProcessor(String p) {
            synchronized (beanPostProcessors) {
                beanPostProcessors.remove(p);
                beanPostProcessors.add(p);
            }
        }

        void addBeanPostProcessors(Collection<String> ps) {
            synchronized (beanPostProcessors) {
                beanPostProcessors.removeAll(ps);
                beanPostProcessors.addAll(ps);
            }
        }
    }

    static final class Sub extends CopyOnWriteArrayList<String> {
        private static final long serialVersionUID = 1L;

        Sub(Collection<String> c) {
            super(c);
        }
    }

    static final class Holder implements Serializable {
        private static final long serialVersionUID = 1L;
        final CopyOnWriteArrayList<String> list = new CopyOnWriteArrayList<>(List.of("b", "a"));
    }

    /** JDK bodies that all read `lock`; the list, sorted, or the exception. */
    static String jdk(CopyOnWriteArrayList<String> l) {
        try {
            l.addAllAbsent(List.of("k"));
            l.removeAll(List.of("zz"));
            l.retainAll(new ArrayList<>(l));
            l.replaceAll(s -> s);
            l.subList(0, l.size()).size();
            l.sort(null);
            return l.toString();
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static Object roundTrip(Object o) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(o);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return in.readObject();
        }
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Throwable {
        // 1. Spring's BeanPostProcessor list, the 2026-07-14 shape.
        Factory f = new Factory();
        f.addBeanPostProcessors(List.of("a", "b", "c"));
        f.addBeanPostProcessor("b");
        f.addBeanPostProcessors(List.of("c", "d"));
        List<String> bpp = f.beanPostProcessors;
        String afterAdds = bpp.toString();
        String springJdk;
        try {
            bpp.addAll(1, List.of("x"));
            bpp.retainAll(List.of("a", "b", "c", "d"));
            bpp.replaceAll(String::toUpperCase);
            bpp.removeIf("B"::equals);
            bpp.sort(Comparator.reverseOrder());
            ((CopyOnWriteArrayList<String>) bpp).addAllAbsent(List.of("A", "E"));
            ((CopyOnWriteArrayList<String>) bpp).addIfAbsent("F");
            springJdk = bpp.toString();
        } catch (RuntimeException e) {
            springJdk = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println("spring-bpp-list: " + afterAdds + " jdk=" + springJdk
                + " resets=" + f.resets + " cache=" + f.cache);

        // 2. Each stand-in (row off), then the JDK bodies.
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>(List.of("b", "a"));
        l.addIfAbsent("c");
        System.out.println("after-addIfAbsent: jdk=" + jdk(l));

        l = new CopyOnWriteArrayList<>(List.of("b"));
        l.addAll(List.of("a"));
        System.out.println("after-addAll: jdk=" + jdk(l));

        l = new CopyOnWriteArrayList<>(List.of("b", "x", "a"));
        l.removeIf("x"::equals);
        System.out.println("after-removeIf: jdk=" + jdk(l));

        l = new CopyOnWriteArrayList<>(List.of("b", "a"));
        Iterator<String> it = l.iterator();
        System.out.println("after-readers: contains=" + l.contains("a") + " indexOf=" + l.indexOf("a")
                + " get=" + l.get(1) + " size=" + l.size() + " isEmpty=" + l.isEmpty()
                + " next=" + it.next() + " toArray=" + l.toArray().length + " jdk=" + jdk(l));

        l = new CopyOnWriteArrayList<>();
        l.add("b");
        l.add(0, "z");
        l.set(0, "a");
        l.add("y");
        l.remove(2);
        boolean removedQ = l.remove("q");
        System.out.println("after-mutators: " + l + " removed(q)=" + removedQ + " jdk=" + jdk(l));

        l = new CopyOnWriteArrayList<>(List.of("q"));
        l.clear();
        l.add("b");
        System.out.println("after-clear: jdk=" + jdk(l));

        // 3. Every constructor.
        System.out.println("ctor(): jdk=" + jdk(new CopyOnWriteArrayList<>()));
        System.out.println("ctor(ArrayList): jdk=" + jdk(new CopyOnWriteArrayList<>(new ArrayList<>(List.of("b", "a")))));
        CopyOnWriteArrayList<String> source = new CopyOnWriteArrayList<>(List.of("b", "a"));
        System.out.println("ctor(COWAL): jdk=" + jdk(new CopyOnWriteArrayList<>(source)) + " source=" + source);
        System.out.println("ctor(Object[]): jdk=" + jdk(new CopyOnWriteArrayList<>(new String[] {"b", "a"})));

        // 4. Reflective and method-handle construction.
        CopyOnWriteArrayList<String> r0 = CopyOnWriteArrayList.class.getConstructor().newInstance();
        r0.add("b");
        System.out.println("reflect ctor(): jdk=" + jdk(r0));
        CopyOnWriteArrayList<String> r1 = CopyOnWriteArrayList.class.getConstructor(Collection.class)
                .newInstance(List.of("b", "a"));
        System.out.println("reflect ctor(Collection): jdk=" + jdk(r1));
        MethodHandle mh = MethodHandles.publicLookup()
                .findConstructor(CopyOnWriteArrayList.class, MethodType.methodType(void.class));
        CopyOnWriteArrayList<String> m0 = (CopyOnWriteArrayList<String>) mh.invoke();
        m0.add("b");
        System.out.println("method-handle ctor(): jdk=" + jdk(m0));

        // 5. clone() and deserialization (both through resetLock()).
        Sub sub = new Sub(List.of("b", "a"));
        Sub cl = (Sub) sub.clone();
        cl.add("c");
        System.out.println("subclass clone: jdk=" + jdk(cl) + " original=" + sub
                + " class=" + cl.getClass().getSimpleName());
        Sub de = (Sub) roundTrip(sub);
        de.add("c");
        System.out.println("subclass deserialized: jdk=" + jdk(de) + " class=" + de.getClass().getSimpleName());
        Holder h = (Holder) roundTrip(new Holder());
        h.list.add("c");
        System.out.println("list deserialized in a holder: jdk=" + jdk(h.list));

        // 6. Through the List / Collection static types (`native-collections`
        //    registers `List.add` / `size` / `get` natives on the interface;
        //    a non-empty list shows whether one of them answers a COWAL as if
        //    it were ArrayList-shaped).
        List<String> li = new CopyOnWriteArrayList<>(List.of("b"));
        li.add("c");
        li.add(0, "a");
        li.remove("q");
        Collection<String> co = li;
        co.addAll(List.of("d"));
        co.remove("d");
        System.out.println("interface: " + li + " size=" + li.size() + " get(1)=" + li.get(1)
                + " jdk=" + jdk((CopyOnWriteArrayList<String>) li));

        // 7. A CopyOnWriteArraySet's list (addIfAbsent, addAllAbsent, remove,
        //    bulkRemove, retainAll underneath).
        CopyOnWriteArraySet<String> set = new CopyOnWriteArraySet<>();
        set.add("b");
        set.add("a");
        boolean again = set.add("b");
        set.addAll(List.of("c", "a"));
        set.remove("c");
        set.removeIf("zz"::equals);
        set.retainAll(List.of("a", "b"));
        System.out.println("cowas: " + set + " add(b)=" + again);

        // 8. subList (COWSubList locks the parent's `lock`).
        l = new CopyOnWriteArrayList<>(List.of("a", "b", "c", "d"));
        l.subList(1, 3).clear();
        l.subList(0, 1).add("x");
        System.out.println("subList: " + l);

        // 9. A JDK-internal COWAL (Logger.handlers).
        Logger lg = Logger.getAnonymousLogger();
        lg.setUseParentHandlers(false);
        Handler handler = new Handler() {
            @Override
            public void publish(LogRecord record) {
            }

            @Override
            public void flush() {
            }

            @Override
            public void close() {
            }
        };
        lg.addHandler(handler);
        int added = lg.getHandlers().length;
        lg.removeHandler(handler);
        System.out.println("jul-handlers: added=" + added + " removed=" + lg.getHandlers().length);

        // 10. No constructor at all: `lock` is null on HotSpot too.
        Field uf = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        uf.setAccessible(true);
        Object unsafe = uf.get(null);
        CopyOnWriteArrayList<String> raw = (CopyOnWriteArrayList<String>) unsafe.getClass()
                .getMethod("allocateInstance", Class.class)
                .invoke(unsafe, CopyOnWriteArrayList.class);
        String rawAdd;
        try {
            raw.add("x");
            rawAdd = "no exception";
        } catch (RuntimeException e) {
            rawAdd = e.getClass().getName() + " mentions-lock="
                    + (e.getMessage() != null && e.getMessage().contains("lock"));
        }
        System.out.println("unsafe-allocated: add=" + rawAdd);

        // 11. The list's monitor held by this thread: every mutator still
        //     completes (the JDK locks `lock`, never the list).
        CopyOnWriteArrayList<String> mon = new CopyOnWriteArrayList<>(List.of("a", "b", "c", "d", "e", "f"));
        String[] names = {"add", "add(int)", "set", "remove(int)", "remove(Object)", "addIfAbsent", "addAll"};
        Runnable[] ops = {
            () -> mon.add("x"),
            () -> mon.add(0, "p"),
            () -> mon.set(0, "s"),
            () -> mon.remove(0),
            () -> mon.remove("c"),
            () -> mon.addIfAbsent("y"),
            () -> mon.addAll(List.of("z")),
        };
        boolean[] done = new boolean[ops.length];
        Thread[] threads = new Thread[ops.length];
        boolean holds;
        synchronized (mon) {
            for (int i = 0; i < ops.length; i++) {
                final int k = i;
                threads[i] = new Thread(() -> {
                    ops[k].run();
                    done[k] = true;
                }, "list-monitor-" + names[i]);
                threads[i].setDaemon(true);
                threads[i].start();
            }
            long deadline = System.nanoTime() + 3_000_000_000L;
            for (Thread t : threads) {
                long left = (deadline - System.nanoTime()) / 1_000_000L;
                t.join(Math.max(1L, left));
            }
            holds = Thread.holdsLock(mon);
        }
        StringBuilder sb = new StringBuilder("list-monitor:");
        for (int i = 0; i < ops.length; i++) {
            sb.append(' ').append(names[i]).append('=').append(done[i]);
        }
        sb.append(" holds=").append(holds);
        System.out.println(sb);
        for (Thread t : threads) {
            t.join(5_000);
        }
        System.out.println("done");
    }
}
