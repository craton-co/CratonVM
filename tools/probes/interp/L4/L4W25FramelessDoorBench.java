// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 25, lane L4: two frameless-door changes in
// `vm/src/runtime/interpreter/`.
//
// 1. `invoke_fast.rs`, `TRIVIAL_CTOR_ELISION`: the non-virtual door answers
//    javac's default constructor of a direct `Object` subclass
//    (`aload_0; invokespecial Object.<init>; return`) by popping the receiver,
//    with no frame (proposal
//    `i24-L4-proposal-frameless-trivial-constructor-in-the-special-door`).
// 2. `dispatch_virtual.rs`, `execute_invokevirtual_fast_door`: the virtual
//    door BORROWS its inline-cache entry through every decline and both
//    frameless answers (the trivial getter, the empty body), so those calls
//    take no `Arc` clone and drop (two atomic read-modify-writes); the clone
//    is taken only for a frame push.
//
// Rows (ns/call on stderr):
//
//   new-object      - CONTROL: `new Object()`, frameless since wave 20
//   new-default     - `new Plain()`, a default constructor: EXPECTED to drop
//                     towards new-object (one frame push/pop and one door
//                     pass fewer per `new`)
//   new-field-init  - CONTROL: `new WithInit()` (`int x = 7;` in the
//                     constructor: not trivial, framed as before)
//   new-sub-default - CONTROL: a default constructor of a subclass of
//                     `Plain` (calls `Plain.<init>`, not `Object.<init>`:
//                     out of scope, framed as before; its nested
//                     `Plain.<init>` IS elided, so a small drop is expected)
//   getter-mono     - a monomorphic trivial getter through the virtual door:
//                     EXPECTED a few ns faster (no clone)
//   empty-virtual   - a monomorphic empty virtual method: EXPECTED a few ns
//                     faster (no clone)
//   virtual-body    - CONTROL: a virtual method with a body (framed; clones
//                     for the push as before): flat
//   getter-poly     - CONTROL: a bimorphic trivial getter (the poly entry is
//                     owned already): flat
//
// How to run (each row's loop runs in ONE invocation and must stay
// interpreted):
//
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L4W25FramelessDoorBench
//
// A/B: interleave against the wave-24 build (`f5cc0e32d`), fat-LTO build,
// medians of 5. A step of a few ns on the no-LTO build can be a layout
// artefact; settle it on fat LTO. Census of the constructor elision:
// `CRATONVM_DBG_FIELD_SITE=1` prints `[invoke-door] trivial constructors:
// elided=N framed=M` (new-default and new-sub-default's nested call should
// make N >= 2 * 1_020_000; a zero means the rule never engaged).
//
// Stdout is a deterministic checksum per row and must equal HotSpot 25's,
// with and without `--nojit`. HotSpot 25 (25.0.3) prints:
//
//   new-object 1500000
//   new-default 500000
//   new-field-init 7500000
//   new-sub-default 500000
//   getter-mono 3000000
//   empty-virtual 3500000
//   virtual-body 1000000000000
//   getter-poly 4000000
//
// For scale, HotSpot 25 `-Xint` on the i7-8550U box: new-object 125,
// new-default 298, new-field-init 313, new-sub-default 310, getter-mono 96,
// empty-virtual 159, virtual-body 152, getter-poly 133 ns/call.
public class L4W25FramelessDoorBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 1_000_000;

    static class Plain {
        int tag;
    }

    static final class SubPlain extends Plain {
    }

    static final class WithInit {
        int x = 7;
    }

    static class Box {
        int v;

        Box(int v) {
            this.v = v;
        }

        int get() {
            return v;
        }

        void hook(int x) {
        }

        int twice(int x) {
            return x + x + 1;
        }
    }

    static final class OtherBox extends Box {
        OtherBox(int v) {
            super(v);
        }

        @Override
        int get() {
            return v;
        }
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
        System.err.printf("%-16s %8.1f ns/call%n", name, (double) ns / ITERS);
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

    static long newFieldInit(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            WithInit w = new WithInit();
            sum += w.x + (i & 1);
        }
        return sum;
    }

    static long newSubDefault(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            SubPlain p = new SubPlain();
            sum += p.tag + (i & 1);
        }
        return sum;
    }

    static long getterMono(Box b, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += b.get();
        }
        return sum;
    }

    static long emptyVirtual(Box b, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            b.hook(i);
            sum += i & 7;
        }
        return sum;
    }

    static long virtualBody(Box b, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += b.twice(i);
        }
        return sum;
    }

    static long getterPoly(Box a, Box b, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += ((i & 1) == 0 ? a : b).get();
        }
        return sum;
    }

    public static void main(String[] args) {
        Box box = new Box(3);
        Box other = new OtherBox(5);
        row("new-object", L4W25FramelessDoorBench::newObject);
        row("new-default", L4W25FramelessDoorBench::newDefault);
        row("new-field-init", L4W25FramelessDoorBench::newFieldInit);
        row("new-sub-default", L4W25FramelessDoorBench::newSubDefault);
        row("getter-mono", n -> getterMono(box, n));
        row("empty-virtual", n -> emptyVirtual(box, n));
        row("virtual-body", n -> virtualBody(box, n));
        row("getter-poly", n -> getterPoly(box, other, n));
    }
}
