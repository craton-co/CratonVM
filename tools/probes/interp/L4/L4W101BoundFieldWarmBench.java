// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L4: bench and behaviour probe for
// `CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER` (default off; item 1 of
// `docs/internal/proposals/interpreter/i100-L4-proposal-method-handle-warm-call-directions-20261202.md`,
// page `interpreter-i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-FIXED-20261018.md`,
// "Wave 101").
//
// `getter.bindTo(box)` is a `BoundMethodHandle$Species_LL` over the JDK's
// reinvoker form; every warm call ran the JDK invoker linker and the whole
// LambdaForm chain (~19 us/op against HotSpot's few ns). With the switch on, a
// bound handle the JDK's own `bindTo` proves (same species, form, type, argL0,
// argL1) is served from the accessor's field record with argL1 as the base.
//
// Behaviour lines (every arm, both modes, switch on and off, must equal
// HotSpot) pin the hazards of the recogniser:
//   look-alike   `insertArguments(refSetter, 1, b)` has bindTo's species, fields
//                and type but binds the VALUE (a.next = b), where
//                `refSetter.bindTo(b)` binds the BASE (b.next = a);
//   static       a static setter's bindTo binds the value, never a base;
//   alternate    two bound handles of one accessor at one site;
//   after-gc     a collection between warm calls;
//   null-base    bindTo(null): checkBase's NPE on every call;
//   wrong-type   another call-site type still refuses;
//   iface-value  a bound interface-typed setter given an implementing value.
//
// Timing lines start with "TIME " (filter them before diffing):
//   TIME <row> med <ns> ns/op      (median of reps, 20000 calls per rep)
//
// Run: javac -d . L4W101BoundFieldWarmBench.java
//      java -cp . L4W101BoundFieldWarmBench                                   (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W101BoundFieldWarmBench [reps]
//      CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER=1 cratonvm --jdk-only [--nojit] -cp . L4W101BoundFieldWarmBench [reps]
//      (the same with --compatible; interleave the arms, take medians;
//       CRATONVM_DBG=mh-finder-census counts `lane bound-field-before-linker`)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources; TIME lines removed)
// bound-getter 7 8
// bound-setter 11
// look-alike insert a.next=b b.next=null
// look-alike bindTo a.next=null b.next=a
// bound-ref-getter b
// static-bound x
// alternate 7/70 7/70 7/70
// after-gc 7 9
// null-base java.lang.NullPointerException java.lang.NullPointerException
// wrong-type java.lang.invoke.WrongMethodTypeException
// iface-value ran
// checksum bound-getter-warm-exact 140000
// checksum bound-getter-warm-invoke 140000
// checksum bound-setter-warm-exact 20000
// checksum bound-ref-getter-warm-exact 20000
// checksum bound-alternate-warm-exact 770000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L4W101BoundFieldWarmBench {
    public static class Box {
        public int v = 7;
        public int w = 70;
    }

    public static class Node {
        public final String name;
        public Node next;
        public Runnable task;
        public static Object S;

        Node(String name) {
            this.name = name;
        }
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static void row(String name, int reps, int n, Body body) throws Throwable {
        long[] t = new long[reps];
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = body.run(n);
            t[r] = (System.nanoTime() - t0) / n;
            if (r == 0) {
                sum = s;
            }
        }
        System.out.printf(Locale.ROOT, "TIME %s med %.1f ns/op%n", name, median(t));
        System.out.println("checksum " + name + " " + sum);
    }

    /** One call site for every `()I` bound handle. */
    static int read(MethodHandle bound) throws Throwable {
        return (int) bound.invokeExact();
    }

    /** One call site for every `(Node)void` handle. */
    static void link(MethodHandle h, Node arg) throws Throwable {
        h.invokeExact(arg);
    }

    static String nameOf(Node n) {
        return n == null ? "null" : n.name;
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle getter = lookup.findGetter(Box.class, "v", int.class);
        MethodHandle getterW = lookup.findGetter(Box.class, "w", int.class);
        MethodHandle setter = lookup.findSetter(Box.class, "v", int.class);
        MethodHandle nextSetter = lookup.findSetter(Node.class, "next", Node.class);
        MethodHandle nextGetter = lookup.findGetter(Node.class, "next", Node.class);
        MethodHandle taskSetter = lookup.findSetter(Node.class, "task", Runnable.class);
        MethodHandle staticSetter = lookup.findStaticSetter(Node.class, "S", Object.class);
        Box box = new Box();

        MethodHandle bound = getter.bindTo(box);
        int first = read(bound);
        box.v = 8;
        int second = read(bound);
        box.v = 7;
        System.out.println("bound-getter " + first + " " + second);

        MethodHandle boundSetter = setter.bindTo(box);
        boundSetter.invokeExact(11);
        boundSetter.invokeExact(11);
        System.out.println("bound-setter " + box.v);
        box.v = 7;

        // The look-alike pair, each called three times at one site.
        Node a1 = new Node("a");
        Node b1 = new Node("b");
        MethodHandle inserted = MethodHandles.insertArguments(nextSetter, 1, b1);
        for (int i = 0; i < 3; i++) {
            link(inserted, a1);
        }
        System.out.println("look-alike insert a.next=" + nameOf(a1.next) + " b.next=" + nameOf(b1.next));
        Node a2 = new Node("a");
        Node b2 = new Node("b");
        MethodHandle boundNext = nextSetter.bindTo(b2);
        for (int i = 0; i < 3; i++) {
            link(boundNext, a2);
        }
        System.out.println("look-alike bindTo a.next=" + nameOf(a2.next) + " b.next=" + nameOf(b2.next));

        Node holder = new Node("h");
        holder.next = b1;
        MethodHandle boundRef = nextGetter.bindTo(holder);
        Node got = null;
        for (int i = 0; i < 3; i++) {
            got = (Node) boundRef.invokeExact();
        }
        System.out.println("bound-ref-getter " + nameOf(got));

        MethodHandle staticBound = staticSetter.bindTo("x");
        for (int i = 0; i < 3; i++) {
            staticBound.invokeExact();
        }
        System.out.println("static-bound " + Node.S);

        // Two bound handles of two accessors alternating at ONE site.
        MethodHandle boundW = getterW.bindTo(box);
        StringBuilder alt = new StringBuilder("alternate");
        for (int i = 0; i < 3; i++) {
            alt.append(' ').append(read(bound)).append('/').append(read(boundW));
        }
        System.out.println(alt);

        read(bound);
        System.gc();
        box.v = 9;
        int afterGc = read(bound);
        box.v = 7;
        System.out.println("after-gc " + read(bound) + " " + afterGc);

        MethodHandle nullBound = getter.bindTo(null);
        StringBuilder nulls = new StringBuilder("null-base");
        for (int i = 0; i < 2; i++) {
            try {
                read(nullBound);
                nulls.append(" no-throw");
            } catch (NullPointerException e) {
                nulls.append(' ').append(e.getClass().getName());
            }
        }
        System.out.println(nulls);

        try {
            Object o = (Object) bound.invokeExact();
            System.out.println("wrong-type no-throw " + o);
        } catch (Throwable t) {
            System.out.println("wrong-type " + t.getClass().getName());
        }

        Node t1 = new Node("t");
        MethodHandle boundTask = taskSetter.bindTo(t1);
        final boolean[] ran = {false};
        for (int i = 0; i < 3; i++) {
            boundTask.invokeExact((Runnable) () -> ran[0] = true);
        }
        t1.task.run();
        System.out.println("iface-value " + (ran[0] ? "ran" : "not-run"));

        final int n = 20000;
        row("bound-getter-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                sum += (int) bound.invokeExact();
            }
            return sum;
        });
        row("bound-getter-warm-invoke", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                sum += (int) bound.invoke();
            }
            return sum;
        });
        row("bound-setter-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                boundSetter.invokeExact(7);
                sum += 1;
            }
            return sum;
        });
        row("bound-ref-getter-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                Node x = (Node) boundRef.invokeExact();
                sum += x == b1 ? 1 : 0;
            }
            return sum;
        });
        row("bound-alternate-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i += 2) {
                sum += read(bound);
                sum += read(boundW);
            }
            return sum;
        });
    }
}
