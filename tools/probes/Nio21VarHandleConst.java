// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.CountDownLatch;
import java.util.function.Function;

/**
 * nio21 wave 11, lane L5: field accesses through a constant {@code static final VarHandle}
 * answered inline in compiled code (proposal W9L2-2 in
 * docs/internal/proposals/jit/jit-r21-nio-proposals-w9-L2.md; page
 * docs/internal/fixed-bugs/r21-varhandle-constant-handle-field-access-inline-FIXED-20261012.md).
 *
 * <ul>
 *   <li>{@code CRATONVM_JIT_VH_CONST_INLINE} (default ON): int / long fields, every access mode.
 *       Rows {@code prim}, {@code sub}, {@code race}.</li>
 *   <li>{@code CRATONVM_JIT_VH_CONST_REF_INLINE} (default ON): reference fields -- reads, gated
 *       stores, CAS, compareAndExchange, getAndSet. Rows {@code ref}, {@code gc}, {@code cf},
 *       {@code conc}, {@code race}.</li>
 *   <li>nio21 wave 12: {@code CRATONVM_JIT_VH_CONST_SUBTYPE} (subclass receivers and subclass
 *       values through the VM's supers display) and {@code CRATONVM_JIT_IR_VH_CONST_ADMIT} (methods
 *       with such sites reach the optimizing tier). Row {@code abs} (an abstract coordinate class and
 *       field type, the {@code Completion.NEXT} shape, and a {@code MinimalFuture}-like subclass);
 *       bench rows {@code cf}, {@code conc}, {@code vhsub}.</li>
 *   <li>nio21 wave 14: {@code CRATONVM_JIT_VH_CONST_IFACE} (interface-typed field values through
 *       the displays' interface sets) and displays published on first need. Row {@code iface}
 *       ({@code iface sum}, and {@code early sum}: a subclass used inside its own
 *       {@code <clinit>}), {@code exc task*} rows; bench row {@code vhiface}.</li>
 *   <li>Row {@code exc}: every exception a VarHandle access raises at a warmed (compiled) site --
 *       null receiver, wrong receiver class, wrong value class, wrong expected class, a final
 *       field's write modes, numeric modes on a reference -- printed as class and message.</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21VarHandleConst.java
 *   java                  -cp /tmp/p21 Nio21VarHandleConst > hs.txt
 *   cratonvm --jdk-only   -cp /tmp/p21 Nio21VarHandleConst > jo.txt
 *   cratonvm --compatible -cp /tmp/p21 Nio21VarHandleConst > co.txt
 *   CRATONVM_JIT_VH_CONST_INLINE=0 cratonvm --jdk-only -cp /tmp/p21 Nio21VarHandleConst > jo0.txt
 *   CRATONVM_JIT_VH_CONST_REF_INLINE=0 cratonvm --compatible -cp /tmp/p21 Nio21VarHandleConst > co0.txt
 *   # hs = jo = co = jo0 = co0
 *   CRATONVM_DBG_JITC=1 cratonvm --jdk-only -cp /tmp/p21 Nio21VarHandleConst 2>&1 >/dev/null \
 *       | grep -c 'vh-const-inline'                                       # engagement, > 0
 *   java     -cp /tmp/p21 -Dnio21.bench=1 Nio21VarHandleConst             # interleave, 5 runs each
 *   cratonvm -cp /tmp/p21 -Dnio21.bench=1 Nio21VarHandleConst
 *   CRATONVM_JIT_VH_CONST_INLINE=0 cratonvm -cp /tmp/p21 -Dnio21.bench=1 Nio21VarHandleConst
 * </pre>
 *
 * {@code -Dnio21.rows=a,b} selects rows (default all). Conformance prints answers only; the bench
 * ({@code -Dnio21.bench=1} or {@code true}) prints one {@code <row> <N> ns/op} line per row, the
 * median of five timed passes after three warm-up passes ({@code -Dnio21.iters=N}, default 200000).
 */
public class Nio21VarHandleConst {
    static final String ROWS = System.getProperty("nio21.rows", "all");
    static final int WARM = 30_000;

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    // ---- the holders --------------------------------------------------------------------------

    /** A final coordinate class: int, long and reference fields of every flavour. */
    static final class Cell {
        volatile int i;
        long l;
        volatile Object o;
        Node node;
        Base base;
        final int fin = 7;

        static final VarHandle I;
        static final VarHandle L;
        static final VarHandle O;
        static final VarHandle NODE;
        static final VarHandle BASE;
        static final VarHandle FIN;

        static {
            try {
                MethodHandles.Lookup lk = MethodHandles.lookup();
                I = lk.findVarHandle(Cell.class, "i", int.class);
                L = lk.findVarHandle(Cell.class, "l", long.class);
                O = lk.findVarHandle(Cell.class, "o", Object.class);
                NODE = lk.findVarHandle(Cell.class, "node", Node.class);
                BASE = lk.findVarHandle(Cell.class, "base", Base.class);
                FIN = lk.findVarHandle(Cell.class, "fin", int.class);
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }

        // Accessors compiled with the constant handles; each is one site.
        int getI() { return (int) I.get(this); }
        int getIAcq() { return (int) I.getAcquire(this); }
        void setI(int v) { I.set(this, v); }
        void setIVol(int v) { I.setVolatile(this, v); }
        void setIRel(int v) { I.setRelease(this, v); }
        boolean casI(int e, int v) { return I.compareAndSet(this, e, v); }
        boolean wcasI(int e, int v) { return I.weakCompareAndSetPlain(this, e, v); }
        int caeI(int e, int v) { return (int) I.compareAndExchange(this, e, v); }
        int xaddI(int d) { return (int) I.getAndAdd(this, d); }
        int xchgI(int v) { return (int) I.getAndSet(this, v); }
        int orI(int m) { return (int) I.getAndBitwiseOr(this, m); }
        int andI(int m) { return (int) I.getAndBitwiseAndRelease(this, m); }
        int xorI(int m) { return (int) I.getAndBitwiseXorAcquire(this, m); }

        long getL() { return (long) L.getOpaque(this); }
        long getLVol() { return (long) L.getVolatile(this); }
        void setL(long v) { L.setOpaque(this, v); }
        void setLVol(long v) { L.setVolatile(this, v); }
        boolean casL(long e, long v) { return L.compareAndSet(this, e, v); }
        long caeL(long e, long v) { return (long) L.compareAndExchangeAcquire(this, e, v); }
        long xaddL(long d) { return (long) L.getAndAddRelease(this, d); }
        long xchgL(long v) { return (long) L.getAndSetAcquire(this, v); }

        Object getO() { return O.getAcquire(this); }
        void setO(Object v) { O.setRelease(this, v); }
        void setOVol(Object v) { O.setVolatile(this, v); }
        boolean casO(Object e, Object v) { return O.compareAndSet(this, e, v); }
        Object caeO(Object e, Object v) { return O.compareAndExchange(this, e, v); }
        Object xchgO(Object v) { return O.getAndSet(this, v); }

        Node getNode() { return (Node) NODE.get(this); }
        void setNode(Node v) { NODE.set(this, v); }
        boolean casNode(Node e, Node v) { return NODE.compareAndSet(this, e, v); }

        Base getBase() { return (Base) BASE.getVolatile(this); }
        boolean casBase(Base e, Base v) { return BASE.compareAndSet(this, e, v); }
        void setBase(Base v) { BASE.setVolatile(this, v); }

        int getFin() { return (int) FIN.get(this); }
    }

    static final class Node {
        final int v;
        Node next;
        Node(int v) { this.v = v; }
    }

    abstract static class Base {
        abstract int tag();
    }

    static final class SubA extends Base {
        int tag() { return 1; }
    }

    static final class SubB extends Base {
        int tag() { return 2; }
    }

    /** A NON-final coordinate class and its subclass: the subclass's receivers take the call. */
    static class Open {
        volatile long n;
        volatile Object ref;
        static final VarHandle N;
        static final VarHandle REF;

        static {
            try {
                MethodHandles.Lookup lk = MethodHandles.lookup();
                N = lk.findVarHandle(Open.class, "n", long.class);
                REF = lk.findVarHandle(Open.class, "ref", Object.class);
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }

        long bump(long d) { return (long) N.getAndAdd(this, d); }
        boolean swap(Object e, Object v) { return REF.compareAndSet(this, e, v); }
    }

    static class OpenSub extends Open {
        long extra = 3;
    }

    /**
     * nio21 wave 12: the {@code CompletableFuture.Completion.NEXT} shape -- a handle whose coordinate
     * class AND field type are an abstract class, so every receiver and every stored value is a
     * subclass instance.
     */
    abstract static class Link {
        volatile Link next;
        int depth;
        static final VarHandle NEXT;

        static {
            try {
                NEXT = MethodHandles.lookup().findVarHandle(Link.class, "next", Link.class);
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }

        abstract int kind();
    }

    static final class LinkA extends Link {
        int kind() { return 1; }
    }

    static class LinkB extends Link {
        int kind() { return 2; }
    }

    static final class LinkC extends LinkB {
        long pad = 9;
        int kind() { return 3; }
    }

    /** A {@code MinimalFuture}-like subclass: the HttpClient's own futures. */
    static final class MiniFuture<T> extends CompletableFuture<T> {
        @Override
        public <U> CompletableFuture<U> newIncompleteFuture() {
            return new MiniFuture<>();
        }
    }

    // A site OUTSIDE the coordinate class (no static descriptor proof).
    static boolean linkCas(Link l, Link e, Link v) { return Link.NEXT.compareAndSet(l, e, v); }
    static void linkSet(Link l, Link v) { Link.NEXT.setRelease(l, v); }
    static Link linkGet(Link l) { return (Link) Link.NEXT.getAcquire(l); }
    static Object linkXchg(Link l, Object v) { return Link.NEXT.getAndSet(l, v); }
    static Object linkGetObj(Link l) { return (Object) Link.NEXT.getAcquire(l); }

    // ---- nio21 wave 14: an interface-typed field, and a class used during its own <clinit> ----

    interface Task {
        int run();
    }

    static final class TaskA implements Task {
        public int run() { return 1; }
    }

    interface SubTask extends Task {}

    static class TaskBase implements SubTask {
        public int run() { return 2; }
    }

    static final class TaskC extends TaskBase implements Comparable<TaskC> {
        public int run() { return 3; }
        public int compareTo(TaskC o) { return 0; }
    }

    static final class Slot {
        volatile Task task;
        static final VarHandle TASK;

        static {
            try {
                TASK = MethodHandles.lookup().findVarHandle(Slot.class, "task", Task.class);
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }
    }

    // Object-typed sites outside the coordinate class: the interface screen decides.
    static boolean taskCas(Slot s, Object e, Object v) { return Slot.TASK.compareAndSet(s, e, v); }
    static void taskSet(Slot s, Object v) { Slot.TASK.setRelease(s, v); }
    static Object taskXchg(Slot s, Object v) { return Slot.TASK.getAndSet(s, v); }

    /** A subclass of an initialised coordinate class, used during its OWN {@code <clinit>}. */
    static final class EarlyLink extends Link {
        static final long EARLY_SUM;

        static {
            Link a = new EarlyLink();
            Link b = new EarlyLink();
            long s = 0;
            for (int i = 0; i < WARM; i++) {
                s += linkCas(a, a.next, (i & 1) == 0 ? b : null) ? 1 : 0;
                s += linkXchg(b, a) == null ? 2 : 0;
            }
            EARLY_SUM = s;
        }

        int kind() { return 4; }
    }

    static void iface() {
        Slot s = new Slot();
        Task[] ts = {new TaskA(), new TaskBase(), new TaskC(), () -> 4};
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            Task v = ts[i & 3];
            Object cur = s.task;
            sum += taskCas(s, cur, v) ? 1 : 0;
            sum += taskCas(s, cur, v) ? 10 : 0; // fails unless cur == v
            taskSet(s, ts[(i + 1) & 3]);
            sum += ((Task) taskXchg(s, v)).run();
        }
        System.out.println("iface sum " + sum + " " + s.task.run());
        System.out.println("early sum " + EarlyLink.EARLY_SUM);
    }

    /** Subclass receivers and subclass values through an abstract coordinate class. */
    static void abs() {
        Link[] ls = {new LinkA(), new LinkB(), new LinkC(), new LinkA()};
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            Link r = ls[i & 3];
            Link v = ls[(i + 1) & 3];
            Link cur = linkGet(r);
            sum += linkCas(r, cur, v) ? 1 : 0;
            sum += linkCas(r, cur, null) ? 10 : 0; // fails: r.next is v now
            Object o = linkGetObj(r);
            sum += o == v ? 100 : 0;
            linkSet(r, (i & 4) == 0 ? null : ls[(i + 2) & 3]);
            sum += linkXchg(r, v) == null ? 1000 : 0;
            sum += ((Link) linkGetObj(r)).kind();
        }
        StringBuilder sb = new StringBuilder("abs sum " + sum);
        for (Link l : ls) sb.append(' ').append(l.next == null ? 0 : l.next.kind());
        System.out.println(sb);
        // MiniFuture receivers: CompletableFuture's own RESULT / STACK handles.
        long fs = 0;
        for (int i = 0; i < WARM / 10; i++) {
            MiniFuture<Integer> f = new MiniFuture<>();
            CompletableFuture<Integer> g = f.thenApply(x -> x * 3);
            f.complete(i);
            fs += g.join();
            fs += (g instanceof MiniFuture) ? 1 : 0;
        }
        System.out.println("abs mini " + fs);
    }

    // ---- conformance --------------------------------------------------------------------------

    static long primOps(Cell c, int k) {
        long s = 0;
        c.setI(k);
        s += c.getI();
        c.setIVol(k + 1);
        s += c.getIAcq();
        c.setIRel(k + 2);
        s += c.casI(k + 2, k + 3) ? 1 : 0;
        s += c.casI(k + 2, k + 4) ? 10 : 0;
        s += c.wcasI(k + 3, k + 5) ? 100 : 0;
        s += c.caeI(k + 5, k + 6);
        s += c.caeI(-1, k + 7);
        s += c.xaddI(3);
        s += c.xchgI(k ^ 0x5A5A);
        s += c.orI(0x0F00);
        s += c.andI(0xFFF0);
        s += c.xorI(0x1111);
        s += c.getI();
        c.setL((long) k << 33);
        s += c.getL();
        c.setLVol(-k);
        s += c.getLVol();
        s += c.casL(-k, k * 3L) ? 1 : 0;
        s += c.casL(-k, 0) ? 10 : 0;
        s += c.caeL(k * 3L, Long.MIN_VALUE + k);
        s += c.xaddL(Long.MAX_VALUE);
        s += c.xchgL(k);
        s += c.getFin();
        return s;
    }

    static void prim() {
        Cell c = new Cell();
        long sum = 0;
        for (int i = 0; i < WARM; i++) sum += primOps(c, i - 15_000);
        System.out.println("prim sum " + sum);
        for (int k : new int[] {Integer.MIN_VALUE, -1, 0, 1, 0x7FFF_FFFF}) {
            Cell d = new Cell();
            System.out.println("prim " + k + " " + primOps(d, k) + " i=" + d.i + " l=" + d.l);
        }
    }

    static long refOps(Cell c, Node n1, Node n2, Base a, Base b, Object o) {
        long s = 0;
        c.setO(o);
        s += c.getO() == o ? 1 : 0;
        s += c.casO(o, n1) ? 2 : 0;
        s += c.casO(o, n2) ? 4 : 0;
        s += c.caeO(n1, null) == n1 ? 8 : 0;
        s += c.xchgO("x") == null ? 16 : 0;
        c.setOVol(null);
        c.setNode(n1);
        s += c.getNode().v;
        s += c.casNode(n1, n2) ? 32 : 0;
        s += c.casNode(n1, null) ? 64 : 0;
        s += c.casNode(n2, null) ? 128 : 0;
        c.setBase(a);
        s += c.getBase().tag();
        s += c.casBase(a, b) ? 256 : 0;
        s += c.getBase().tag();
        s += c.casBase(b, null) ? 512 : 0;
        return s;
    }

    static void ref() {
        Cell c = new Cell();
        Node n1 = new Node(5);
        Node n2 = new Node(6);
        Base a = new SubA();
        Base b = new SubB();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += refOps(c, (i & 1) == 0 ? n1 : new Node(i), n2, a, (i & 2) == 0 ? b : new SubA(), "s" + (i & 7));
        }
        System.out.println("ref sum " + sum);
        System.out.println("ref final " + c.getO() + " " + c.getNode() + " " + c.getBase());
    }

    /** Subclass receivers of a non-final coordinate: every access must still be exact. */
    static void sub() {
        Open[] rs = {new Open(), new OpenSub(), new Open(), new OpenSub()};
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            Open r = rs[i & 3];
            sum += r.bump(i);
            Object prev = r.ref;
            sum += r.swap(prev, (i & 1) == 0 ? null : r) ? 1 : 0;
        }
        System.out.println("sub sum " + sum + " " + rs[0].n + " " + rs[1].n + " " + rs[2].n + " " + rs[3].n
                + " " + ((OpenSub) rs[1]).extra);
    }

    /** Long-lived holders receiving young references through CAS / set, across collections. */
    static void gc() {
        Cell[] holders = new Cell[64];
        for (int i = 0; i < holders.length; i++) holders[i] = new Cell();
        System.gc(); // promote the holders where a collector promotes
        long check = 0;
        for (int round = 0; round < 6; round++) {
            for (int i = 0; i < WARM; i++) {
                Cell h = holders[i & 63];
                Node fresh = new Node(i + round);
                Node cur = h.getNode();
                if (!h.casNode(cur, fresh)) h.setNode(fresh);
                h.setO(new int[] {i, round});
                if ((i & 1023) == 0) {
                    byte[][] junk = new byte[64][];
                    for (int j = 0; j < junk.length; j++) junk[j] = new byte[256];
                    check += junk.length;
                }
            }
            System.gc();
            for (Cell h : holders) {
                check += h.getNode().v;
                int[] p = (int[]) h.getO();
                check += p[0] + p[1];
            }
        }
        System.out.println("gc check " + check);
    }

    static void cf() {
        long sum = 0;
        for (int i = 0; i < WARM / 10; i++) {
            CompletableFuture<Integer> f = new CompletableFuture<>();
            CompletableFuture<Integer> g = f.thenApply(x -> x + 1).thenApply(x -> x * 2);
            f.complete(i);
            sum += g.join();
            CompletableFuture<Integer> h = CompletableFuture.completedFuture(i).thenApply(x -> x - 1);
            sum += h.join();
            sum += f.complete(-1) ? 1000 : 0; // already completed: false
        }
        CompletableFuture<String> e = new CompletableFuture<>();
        CompletableFuture<String> m = e.thenApply(s -> s + "!");
        e.completeExceptionally(new IllegalStateException("boom"));
        try {
            m.join();
        } catch (RuntimeException ex) {
            System.out.println("cf exc " + ex.getClass().getName() + " " + ex.getCause().getMessage());
        }
        System.out.println("cf sum " + sum);
    }

    static void conc() {
        ConcurrentLinkedQueue<Integer> q = new ConcurrentLinkedQueue<>();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            q.offer(i);
            if ((i & 3) == 3) {
                sum += q.poll();
                sum += q.poll();
            }
        }
        while (!q.isEmpty()) sum += q.poll();
        System.out.println("conc sum " + sum + " " + q.size());
    }

    /** A Treiber stack on a constant VarHandle: the ARFU-like CAS loop. */
    static final class Stack {
        volatile Node head;
        static final VarHandle HEAD;

        static {
            try {
                HEAD = MethodHandles.lookup().findVarHandle(Stack.class, "head", Node.class);
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }

        void push(Node n) {
            Node h;
            do {
                h = (Node) HEAD.getAcquire(this);
                n.next = h;
            } while (!HEAD.weakCompareAndSet(this, h, n));
        }

        int size() {
            int k = 0;
            for (Node n = head; n != null; n = n.next) k++;
            return k;
        }
    }

    static void race() throws InterruptedException {
        Cell c = new Cell();
        Stack st = new Stack();
        int threads = 4;
        int per = 50_000;
        CountDownLatch go = new CountDownLatch(1);
        List<Thread> ts = new ArrayList<>();
        for (int t = 0; t < threads; t++) {
            Thread th = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                    return;
                }
                for (int i = 0; i < per; i++) {
                    c.xaddI(1);
                    c.xaddL(2);
                    long cur;
                    do {
                        cur = c.getLVol();
                    } while (!c.casL(cur, cur + 1));
                    int k;
                    do {
                        k = c.getIAcq();
                    } while (!c.wcasI(k, k + 1));
                    if ((i & 7) == 0) st.push(new Node(i));
                }
            });
            ts.add(th);
            th.start();
        }
        go.countDown();
        for (Thread th : ts) th.join();
        System.out.println("race i=" + c.i + " l=" + c.l + " stack=" + st.size()
                + " expect " + (2 * threads * per) + " " + (3L * threads * per) + " " + (threads * ((per + 7) / 8)));
    }

    interface Thrower {
        Object run();
    }

    static void tryRow(String name, Thrower t) {
        try {
            System.out.println("exc " + name + " = " + t.run());
        } catch (Throwable e) {
            System.out.println("exc " + name + " " + ex(e));
        }
    }

    // Object-typed call sites: the handle's own asType checks run here.
    static int getIObj(Object r) { return (int) Cell.I.get(r); }
    static boolean casNodeObj(Cell c, Object e, Object v) { return Cell.NODE.compareAndSet(c, e, v); }
    static void setNodeObj(Cell c, Object v) { Cell.NODE.set(c, v); }
    static void setFin(Cell c, int v) { Cell.FIN.set(c, v); }
    static Object addRef(Cell c, Object v) { return Cell.O.getAndAdd(c, v); }

    static void exc() {
        Cell c = new Cell();
        Node n = new Node(1);
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += getIObj(c);
            sum += casNodeObj(c, null, n) ? 1 : 0;
            sum += casNodeObj(c, n, null) ? 1 : 0;
            setNodeObj(c, (i & 1) == 0 ? n : null);
            sum += c.getI();
        }
        System.out.println("exc warm " + sum);
        tryRow("nullGet", () -> getIObj(null));
        tryRow("nullCas", () -> casNodeObj(null, null, n));
        tryRow("wrongRecv", () -> getIObj("not a cell"));
        tryRow("wrongRecvArr", () -> getIObj(new int[1]));
        tryRow("wrongValue", () -> {
            setNodeObj(c, "not a node");
            return c.node;
        });
        tryRow("wrongNew", () -> casNodeObj(c, null, Integer.valueOf(3)));
        tryRow("wrongExpected", () -> casNodeObj(c, "not a node", null));
        tryRow("finalSet", () -> {
            setFin(c, 9);
            return c.fin;
        });
        tryRow("finalGet", () -> c.getFin());
        tryRow("numericOnRef", () -> addRef(c, 1));
        tryRow("after", () -> c.getNode() + " " + c.getI());
        // nio21 wave 12: an abstract coordinate class / field type, warmed on
        // subclass instances, then handed a value of another type.
        Link l = new LinkC();
        for (int i = 0; i < WARM; i++) {
            linkXchg(l, (i & 1) == 0 ? new LinkA() : null);
        }
        tryRow("linkWrongValue", () -> linkXchg(l, "not a link"));
        tryRow("linkWrongRecv", () -> {
            Object o = "not a link";
            return Link.NEXT.getAndSet(o, (Object) null);
        });
        tryRow("linkNullRecv", () -> linkXchg(null, null));
        tryRow("linkAfter", () -> l.next == null ? "null" : String.valueOf(l.next.kind()));
        // nio21 wave 14: an interface-typed field warmed on implementors, then
        // handed a non-implementor as the new and as the expected value.
        Slot slot = new Slot();
        for (int i = 0; i < WARM; i++) {
            taskCas(slot, slot.task, (i & 1) == 0 ? new TaskA() : new TaskC());
        }
        tryRow("taskWrongNew", () -> taskCas(slot, slot.task, "not a task"));
        tryRow("taskWrongExpected", () -> taskCas(slot, "not a task", null));
        tryRow("taskWrongSet", () -> {
            taskSet(slot, Integer.valueOf(5));
            return slot.task.run();
        });
        tryRow("taskAfter", () -> slot.task.run());
    }

    // ---- bench --------------------------------------------------------------------------------

    interface Op {
        long run(int i);
    }

    static final List<String> TIMES = new ArrayList<>();

    static void bench(String name, int n, Op op) {
        if (!on(name)) return;
        long sink = 0;
        for (int w = 0; w < 3; w++) for (int i = 0; i < n; i++) sink += op.run(i);
        long[] t = new long[5];
        for (int r = 0; r < t.length; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) sink += op.run(i);
            t[r] = System.nanoTime() - t0;
        }
        Arrays.sort(t);
        TIMES.add(name + " " + (t[2] / n) + " ns/op" + (sink == 42 ? " " : ""));
    }

    public static void main(String[] args) throws Exception {
        String b = System.getProperty("nio21.bench", "");
        if (b.equals("1") || b.equalsIgnoreCase("true")) {
            int n = Integer.getInteger("nio21.iters", 200_000);
            Cell c = new Cell();
            Node n1 = new Node(1);
            Node n2 = new Node(2);
            Stack st = new Stack();
            bench("vhget", n, i -> c.getI() + c.getL());
            bench("vhset", n, i -> {
                c.setIRel(i);
                c.setLVol(i);
                return 1;
            });
            bench("vhcas", n, i -> c.casI(c.i, i) ? 1 : 0);
            bench("vhxadd", n, i -> c.xaddL(i) + c.xaddI(1));
            bench("vhrefget", n, i -> c.getO() == null ? 1 : 0);
            bench("vhrefcas", n, i -> c.casNode(c.node, (i & 1) == 0 ? n1 : n2) ? 1 : 0);
            bench("vhpush", n, i -> {
                if ((i & 1023) == 0) st.head = null;
                st.push(new Node(i));
                return 1;
            });
            Function<Integer, Integer> inc = x -> x + 1;
            bench("cfcomplete", n, i -> {
                CompletableFuture<Integer> f = new CompletableFuture<>();
                f.complete(i);
                return f.join();
            });
            bench("cfthen", n / 4, i -> {
                CompletableFuture<Integer> f = new CompletableFuture<>();
                CompletableFuture<Integer> g = f.thenApply(inc).thenApply(inc);
                f.complete(i);
                return g.join();
            });
            ConcurrentLinkedQueue<Integer> q = new ConcurrentLinkedQueue<>();
            bench("clq", n, i -> {
                q.offer(i);
                return q.poll();
            });
            // nio21 wave 12: the census rows' shapes. `cf`: a MinimalFuture-like
            // subclass completing a two-stage chain (subclass receivers of
            // RESULT / STACK, Completion values of STACK / NEXT); `conc`: a
            // ConcurrentLinkedDeque used from both ends.
            bench("cf", n / 4, i -> {
                MiniFuture<Integer> f = new MiniFuture<>();
                CompletableFuture<Integer> g = f.thenApply(inc).thenApply(inc);
                f.complete(i);
                return g.join();
            });
            java.util.concurrent.ConcurrentLinkedDeque<Integer> dq =
                    new java.util.concurrent.ConcurrentLinkedDeque<>();
            bench("conc", n, i -> {
                dq.offerLast(i);
                dq.offerFirst(i);
                return dq.pollFirst() + dq.pollLast();
            });
            Link[] links = {new LinkA(), new LinkB(), new LinkC()};
            bench("vhsub", n, i -> {
                Link r = links[i % 3];
                return linkCas(r, r.next, links[(i + 1) % 3]) ? 1 : 0;
            });
            // nio21 wave 14: an interface-typed field's CAS.
            Slot slot = new Slot();
            Task[] tasks = {new TaskA(), new TaskC()};
            bench("vhiface", n, i -> taskCas(slot, slot.task, tasks[i & 1]) ? 1 : 0);
            for (String t : TIMES) System.out.println(t);
            return;
        }
        if (on("prim")) prim();
        if (on("ref")) ref();
        if (on("sub")) sub();
        if (on("abs")) abs();
        if (on("iface")) iface();
        if (on("gc")) gc();
        if (on("cf")) cf();
        if (on("conc")) conc();
        if (on("race")) race();
        if (on("exc")) exc();
    }
}
