// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t2/fp (2026-10-19): Java-visible answers of virtual dispatch when the
// VM builds a method's dispatch snapshot on its FIRST vtable dispatch
// (CRATONVM_VTABLE_LAZY_DISPATCH=1) instead of at link. The shapes are the
// ones whose first use the deferral reorders:
//   - an inherited method first called through a subclass receiver, then
//     through the declaring class (one snapshot per method, either order);
//   - one method first called through many receiver classes in turn
//     (megamorphic first calls), and a method that is never called;
//   - an override that calls super, an interface default first reached
//     through an implementing class, an abstract slot filled lower down;
//   - eight threads racing the very first dispatch of the same methods
//     (the build is published once; every thread must see the same answer);
//   - a recursive virtual method whose first activation is still on the
//     stack when the second dispatch arrives.
// Every stdout line is deterministic and must equal HotSpot's byte for byte.
//
// Usage: Zt2FpLazyDispatch [rounds]   (default 2000)
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;

public class Zt2FpLazyDispatch {
    static class Root {
        int inherited() {
            return 7;
        }

        int overridden() {
            return 100;
        }

        int neverCalled() {
            return -1;
        }

        int depth(int n) {
            return n <= 0 ? 0 : 1 + depth(n - 1);
        }
    }

    static class A extends Root {
        @Override
        int overridden() {
            return super.overridden() + 1;
        }
    }

    static class B extends A {
        @Override
        int overridden() {
            return super.overridden() + 10;
        }
    }

    static class C extends B {}

    static class D extends Root {
        @Override
        int depth(int n) {
            return n <= 0 ? 100 : 1 + depth(n - 1);
        }
    }

    static class E extends Root {}

    static class F extends E {}

    static class G extends F {}

    static class H extends Root {
        @Override
        int inherited() {
            return 70;
        }
    }

    interface Greeter {
        default String greet() {
            return "hi:" + getClass().getSimpleName();
        }
    }

    abstract static class Shape implements Greeter {
        abstract int sides();

        int twice() {
            return 2 * sides();
        }
    }

    static class Tri extends Shape {
        @Override
        int sides() {
            return 3;
        }
    }

    static class Quad extends Shape {
        @Override
        int sides() {
            return 4;
        }

        @Override
        public String greet() {
            return "quad";
        }
    }

    static Root make(int i) {
        switch (i % 8) {
            case 0:
                return new Root();
            case 1:
                return new A();
            case 2:
                return new B();
            case 3:
                return new C();
            case 4:
                return new D();
            case 5:
                return new E();
            case 6:
                return new F();
            default:
                return new G();
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 2000;

        // 1. Inherited, first through the deepest subclass, then upwards.
        System.out.println("inherited g=" + new G().inherited() + " f=" + new F().inherited()
                + " e=" + new E().inherited() + " root=" + new Root().inherited()
                + " h=" + new H().inherited());

        // 2. Override chains with super calls, first through the leaf.
        System.out.println("overridden c=" + new C().overridden() + " b=" + new B().overridden()
                + " a=" + new A().overridden() + " root=" + new Root().overridden());

        // 3. Interface default and an abstract slot filled lower down.
        Shape[] shapes = {new Tri(), new Quad()};
        StringBuilder sb = new StringBuilder("shapes");
        for (Shape s : shapes) {
            sb.append(' ').append(s.greet()).append('/').append(s.twice());
        }
        System.out.println(sb);

        // 4. Recursion: the first activation is live when the next dispatch
        // of the same slot arrives.
        System.out.println("depth root=" + new Root().depth(50) + " d=" + new D().depth(50)
                + " g=" + new G().depth(50));

        // 5. Megamorphic first calls, then a warm loop.
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            Root o = make(r);
            sum += o.inherited() + o.overridden() + o.depth(r & 7);
        }
        System.out.println("mega rounds=" + rounds + " sum=" + sum);

        // 6. Eight threads race the first dispatch of methods nobody has
        // called yet on these receivers (twice() through each class, and
        // neverCalled() through fresh classes).
        final int threads = 8;
        final CountDownLatch start = new CountDownLatch(1);
        final CountDownLatch done = new CountDownLatch(threads);
        final AtomicLong total = new AtomicLong();
        final AtomicLong bad = new AtomicLong();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            Thread th = new Thread(() -> {
                try {
                    start.await();
                    long local = 0;
                    for (int i = 0; i < 500; i++) {
                        Root o = make(id + i);
                        int v = o.neverCalled();
                        if (v != -1) {
                            bad.incrementAndGet();
                        }
                        local += v;
                        Shape s = (i & 1) == 0 ? new Tri() : new Quad();
                        local += s.twice();
                    }
                    total.addAndGet(local);
                } catch (InterruptedException e) {
                    bad.incrementAndGet();
                } finally {
                    done.countDown();
                }
            });
            th.start();
        }
        start.countDown();
        done.await();
        System.out.println("race threads=" + threads + " total=" + total.get() + " bad=" + bad.get());

        // 7. The never-called-on-H method, called once at the very end.
        System.out.println("late h=" + new H().neverCalled());
        System.out.println("zt2fp lazy-dispatch done");
    }
}
