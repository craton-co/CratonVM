// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L6 -- a JDK virtual thread that starts
// with a dead one's interpreter site caches
// (CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES, default off; page
// i93-L6-virtual-thread-start-pays-a-cold-invoke-cache). Every virtual thread
// here is started and joined one at a time, so with the switch on each one
// inherits the caches its predecessor filled at the SAME call sites, and each
// section makes the predecessor's entry the wrong answer for the successor:
// * polymorphic: thread k sees receiver class k % 3 at one interface, one
//   virtual and one getfield site;
// * loaders: four class loaders each define their own copy of Plug (its own
//   static `count`); thread k calls loader k % 4's copy through one interface
//   site, and Plug's getstatic / putstatic sites belong to four classes;
// * init: a class whose initializer throws is first touched by one virtual
//   thread (ExceptionInInitializerError) and then by the next one, which must
//   see NoClassDefFoundError, not a cached initialized class;
// * casts: one checkcast site alternates a passing and a failing receiver.
// The output must be the same with the switch off and on, in both modes, and
// with CRATONVM_VTHREAD_LEAN_REGISTRY=1:
//   cratonvm L6W94VirtualThreadRecycledCaches
//   CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES=1 cratonvm L6W94VirtualThreadRecycledCaches
//   CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES=1 CRATONVM_VTHREAD_LEAN_REGISTRY=1 cratonvm L6W94VirtualThreadRecycledCaches
//   (and each with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN
// polymorphic: area=1600 tags=600 names=Sq,Rect,Tri
// loaders: sum=220 counts=10,10,10,10 distinct=4
// init: first=java.lang.ExceptionInInitializerError second=java.lang.NoClassDefFoundError
// casts: ok=50 cce=50 msg=class L6W94VirtualThreadRecycledCaches$Sq cannot be cast to class L6W94VirtualThreadRecycledCaches$Rect (L6W94VirtualThreadRecycledCaches$Sq and L6W94VirtualThreadRecycledCaches$Rect are in unnamed module of loader 'app')
// virtual=true
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.util.HashSet;
import java.util.Set;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.IntSupplier;

public class L6W94VirtualThreadRecycledCaches {
    interface Shape {
        int area();
    }

    abstract static class Base implements Shape {
        final int tag;

        Base(int tag) {
            this.tag = tag;
        }

        abstract String name();
    }

    static final class Sq extends Base {
        final int s;

        Sq(int s) {
            super(1);
            this.s = s;
        }

        @Override
        public int area() {
            return s * s;
        }

        @Override
        String name() {
            return "Sq";
        }
    }

    static final class Rect extends Base {
        final int w;
        final int h;

        Rect(int w, int h) {
            super(2);
            this.w = w;
            this.h = h;
        }

        @Override
        public int area() {
            return w * h;
        }

        @Override
        String name() {
            return "Rect";
        }
    }

    static final class Tri extends Base {
        final int b;
        final int h;

        Tri(int b, int h) {
            super(3);
            this.b = b;
            this.h = h;
        }

        @Override
        public int area() {
            return b * h / 2;
        }

        @Override
        String name() {
            return "Tri";
        }
    }

    // Defined once per PlugLoader: each copy has its own `count`.
    public static final class Plug implements IntSupplier {
        static int count;

        @Override
        public int getAsInt() {
            return ++count;
        }
    }

    static final String PLUG = "L6W94VirtualThreadRecycledCaches$Plug";

    static final class PlugLoader extends ClassLoader {
        private final byte[] bytes;

        PlugLoader(byte[] bytes, ClassLoader parent) {
            super(parent);
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(PLUG)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                return c;
            }
        }
    }

    static final class Bad {
        static int x = boom();

        static int boom() {
            throw new IllegalStateException("boom");
        }

        Bad() {
        }
    }

    static void runOne(Runnable r) throws InterruptedException {
        Thread.ofVirtual().start(r).join();
    }

    // The sites every section shares across threads.
    static int areaOf(Shape s) {
        return s.area();
    }

    static String nameOf(Base b) {
        return b.name();
    }

    static int tagOf(Base b) {
        return b.tag;
    }

    static int callPlug(IntSupplier p) {
        return p.getAsInt();
    }

    static Rect asRect(Object o) {
        return (Rect) o;
    }

    public static void main(String[] args) throws Exception {
        // polymorphic
        Base[] shapes = {new Sq(2), new Rect(2, 3), new Tri(4, 3)};
        AtomicLong area = new AtomicLong();
        AtomicLong tags = new AtomicLong();
        String[] names = new String[3];
        for (int k = 0; k < 300; k++) {
            final int i = k;
            runOne(() -> {
                Base b = shapes[i % 3];
                area.addAndGet(areaOf(b));
                tags.addAndGet(tagOf(b));
                if (i < 3) {
                    names[i] = nameOf(b);
                }
            });
        }
        System.out.println("polymorphic: area=" + area.get() + " tags=" + tags.get()
                + " names=" + String.join(",", names));

        // loaders
        byte[] plugBytes;
        try (InputStream in = L6W94VirtualThreadRecycledCaches.class.getResourceAsStream(
                "L6W94VirtualThreadRecycledCaches$Plug.class")) {
            plugBytes = in.readAllBytes();
        }
        IntSupplier[] plugs = new IntSupplier[4];
        Set<Class<?>> classes = new HashSet<>();
        for (int j = 0; j < 4; j++) {
            PlugLoader loader = new PlugLoader(plugBytes,
                    L6W94VirtualThreadRecycledCaches.class.getClassLoader());
            Class<?> c = loader.loadClass(PLUG);
            classes.add(c);
            plugs[j] = (IntSupplier) c.getDeclaredConstructor().newInstance();
        }
        AtomicLong plugSum = new AtomicLong();
        int[] last = new int[4];
        for (int k = 0; k < 40; k++) {
            final int i = k;
            runOne(() -> {
                int v = callPlug(plugs[i % 4]);
                plugSum.addAndGet(v);
                last[i % 4] = v;
            });
        }
        System.out.println("loaders: sum=" + plugSum.get() + " counts=" + last[0] + "," + last[1]
                + "," + last[2] + "," + last[3] + " distinct=" + classes.size());

        // init
        String[] init = new String[2];
        for (int k = 0; k < 2; k++) {
            final int i = k;
            runOne(() -> {
                try {
                    new Bad();
                    init[i] = "none";
                } catch (Throwable t) {
                    init[i] = t.getClass().getName();
                }
            });
        }
        System.out.println("init: first=" + init[0] + " second=" + init[1]);

        // casts
        AtomicLong ok = new AtomicLong();
        AtomicLong cce = new AtomicLong();
        AtomicReference<String> msg = new AtomicReference<>();
        for (int k = 0; k < 100; k++) {
            final int i = k;
            runOne(() -> {
                Object o = (i % 2 == 0) ? new Rect(1, 1) : new Sq(1);
                try {
                    asRect(o);
                    ok.incrementAndGet();
                } catch (ClassCastException e) {
                    cce.incrementAndGet();
                    msg.compareAndSet(null, e.getMessage());
                }
            });
        }
        System.out.println("casts: ok=" + ok.get() + " cce=" + cce.get() + " msg=" + msg.get());

        boolean[] virtual = new boolean[1];
        runOne(() -> virtual[0] = Thread.currentThread().isVirtual());
        System.out.println("virtual=" + virtual[0]);
    }
}
