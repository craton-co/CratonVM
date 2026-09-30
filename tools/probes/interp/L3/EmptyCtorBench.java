// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 20, lane L3: the non-virtual invoke door answers
// a call of an empty body (first instruction `return`) by popping its
// arguments, with no frame pushed, while no JVMTI/JDWP observer needs the
// frame (`invoke_fast::EMPTY_BODY_ELISION`, `empty_body_elidable`). Every
// constructor chain ends in `invokespecial Object.<init>()V`, whose body is a
// lone `return`, so every interpreted `new` pays one frame push less.
//
//   new-object    - `new Object()`: the elided call is the whole constructor
//   new-leaf      - a class whose constructor is `super()` only: one frame
//                   (Leaf.<init>) pushed, the `Object.<init>` below it elided
//   new-chain3    - three constructors chained through `super(..)`, fields
//                   stored on the way; the innermost `Object.<init>` elided
//   empty-private - a private `void` method with an empty body, called from
//                   its own class (`invokevirtual` of a private method, the
//                   virtual door's hand-off to the non-virtual door)
//   empty-super   - an override that calls an empty `super.hook(x)`
//
// What to measure: each row's ns/op on stderr, interleaved against the
// previous build (medians of several runs; in-JVM timings swing ~3x). The
// stage should speed up every row with `--nojit` (the loops stay interpreted)
// and leave the JIT-on rows flat or faster (once a loop is OSR-compiled the
// door no longer runs). `CRATONVM_DBG_FIELD_SITE=1` prints
// `[invoke-door] empty-body callees: elided=N framed=M` at exit: `framed`
// should be ~0 without an agent.
//
// Stdout is a deterministic checksum per row and must match HotSpot 25, with
// and without `--nojit`.
public class EmptyCtorBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 400_000;

    static final class Leaf {
        Leaf() {
            super();
        }
    }

    static class A {
        final int a;

        A(int a) {
            this.a = a;
        }
    }

    static class B extends A {
        final int b;

        B(int a, int b) {
            super(a);
            this.b = b;
        }
    }

    static final class C extends B {
        final int c;

        C(int a, int b, int c) {
            super(a, b);
            this.c = c;
        }
    }

    static class Hooked {
        void hook(int x) {
        }
    }

    static final class Overriding extends Hooked {
        int seen;

        @Override
        void hook(int x) {
            super.hook(x);
            seen += x & 7;
        }
    }

    private int calls;

    private void nothing(int x) {
    }

    interface Row {
        long run(int iters);
    }

    static void row(String name, Row r) {
        r.run(WARMUP);
        long t0 = System.nanoTime();
        long sum = r.run(ITERS);
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + sum);
        System.err.printf("%-14s %8.1f ns/op%n", name, (double) ns / ITERS);
    }

    static long newObject(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            Object o = new Object();
            sum += (o != null) ? (i & 3) : 0;
        }
        return sum;
    }

    static long newLeaf(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            Leaf l = new Leaf();
            sum += (l != null) ? (i & 5) : 0;
        }
        return sum;
    }

    static long newChain3(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            C c = new C(i, i >>> 1, i >>> 2);
            sum += c.a - c.b + c.c;
        }
        return sum;
    }

    long emptyPrivate(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            nothing(i);
            sum += i & 9;
        }
        calls += iters;
        return sum + calls;
    }

    static long emptySuper(Overriding o, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            o.hook(i);
        }
        sum += o.seen;
        o.seen = 0;
        return sum;
    }

    public static void main(String[] args) {
        EmptyCtorBench self = new EmptyCtorBench();
        Overriding hooked = new Overriding();
        row("new-object", EmptyCtorBench::newObject);
        row("new-leaf", EmptyCtorBench::newLeaf);
        row("new-chain3", EmptyCtorBench::newChain3);
        row("empty-private", n -> {
            self.calls = 0;
            return self.emptyPrivate(n);
        });
        row("empty-super", n -> emptySuper(hooked, n));
    }
}
