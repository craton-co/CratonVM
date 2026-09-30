// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 24, lane L4: the non-virtual door answers an
// empty body (`Object.<init>` and every other method whose first instruction
// is `return`) on the inline-cache entry it BORROWS, instead of cloning the
// entry's `Arc` and dropping it again (two atomic read-modify-writes per
// call). `vm/src/runtime/interpreter/invoke_fast.rs`:
// `nonvirtual_door_prelude` (borrowed) / `nonvirtual_door_finish` (owned),
// called from `execute_nonvirtual_fast_door` (`invokespecial`) and from the
// virtual door's private hand-off (`dispatch_virtual.rs`).
//
//   new-object     - `new Object()`: one `invokespecial Object.<init>`,
//                    answered without a frame (now without a clone)
//   new-default    - `new Plain()`: a default constructor (framed, cloned as
//                    before) whose nested `Object.<init>` is the elided call
//   empty-private  - an empty private instance method (`invokevirtual`
//                    private: the virtual door's hand-off)
//   super-empty    - `super.hook(i)` of an empty method (`invokespecial`)
//   empty-static   - an empty static method: the static door, which also
//                    borrows its entry until the tier-up block now
//   static-call    - CONTROL: a static method with a body (the static door;
//                    it clones for the push as before)
//
// How to run (each row's loop runs in ONE invocation and must stay
// interpreted):
//
//   cratonvm --nojit EmptyBodyBorrowBench
//
// A/B: interleave against the wave-23 build (`4db772067`), fat-LTO build,
// medians of 5, ns/call on stderr. Expected: new-object, empty-private,
// super-empty and empty-static a few ns faster (the clone and drop of one `Arc` per call);
// new-default slightly faster (its nested `Object.<init>`); static-call flat.
// A step of a few ns on the no-LTO build can be a layout artefact (wave-23
// lesson); settle it on fat LTO.
//
// Stdout is a deterministic checksum per row and must equal HotSpot 25's,
// with and without `--nojit`. HotSpot 25 (25.0.3) prints:
//
//   new-object 1500000
//   new-default 500000
//   empty-private 3500000
//   super-empty 1500000
//   empty-static 2500000
//   static-call 1000000000000
//
// For scale, HotSpot 25 `-Xint` on the i7-8550U box: new-object 45,
// new-default 64, empty-private 37, super-empty 39, static-call 32 ns/call.
public class EmptyBodyBorrowBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 1_000_000;

    static class Base {
        void hook(int x) {
        }
    }

    static final class Derived extends Base {
        @Override
        void hook(int x) {
            throw new AssertionError("the super call must not reach the override");
        }

        long superLoop(int iters) {
            long sum = 0;
            for (int i = 0; i < iters; i++) {
                super.hook(i);
                sum += i & 3;
            }
            return sum;
        }
    }

    static final class Plain {
        int tag;
    }

    private void emptyPriv(int x) {
    }

    static int staticBody(int x) {
        return (x << 1) + 1;
    }

    static void emptyStatic(int x) {
    }

    static long emptyStaticLoop(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            emptyStatic(i);
            sum += i & 5;
        }
        return sum;
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
        System.err.printf("%-15s %8.1f ns/call%n", name, (double) ns / ITERS);
    }

    static long newObject(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            Object o = new Object();
            sum += (o != null ? 1 : 0) + (i & 1);
        }
        return sum;
    }

    static long newDefault(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            Plain p = new Plain();
            sum += p.tag + (i & 1);
        }
        return sum;
    }

    long emptyPrivate(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            emptyPriv(i);
            sum += i & 7;
        }
        return sum;
    }

    static long staticCall(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += staticBody(i);
        }
        return sum;
    }

    public static void main(String[] args) {
        EmptyBodyBorrowBench self = new EmptyBodyBorrowBench();
        Derived d = new Derived();
        row("new-object", EmptyBodyBorrowBench::newObject);
        row("new-default", EmptyBodyBorrowBench::newDefault);
        row("empty-private", self::emptyPrivate);
        row("super-empty", d::superLoop);
        row("empty-static", EmptyBodyBorrowBench::emptyStaticLoop);
        row("static-call", EmptyBodyBorrowBench::staticCall);
    }
}
