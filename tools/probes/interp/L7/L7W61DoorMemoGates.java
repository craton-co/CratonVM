// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L7: the kill-switch memos left on the
// door and return paths move into per-VM state, read once
// (`VmConfig::frame_slot_reuse` for `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`,
// `frame_emplace` for `CRATONVM_JIT_NO_FRAME_EMPLACE`, `jit_virtual_tierup`
// for `CRATONVM_JIT_VIRTUAL_TIERUP`, `door_receiver_record` for
// `CRATONVM_JIT_NO_DOOR_RECEIVER_RECORD`, `door_recv_memo` for
// `CRATONVM_JIT_NO_DOOR_RECV_MEMO`; `ClassRealm::ann_proxy_latch` for
// `CRATONVM_LOADER_NO_ANN_PROXY_LATCH`); the virtual door's private hand-off
// tests the dispatch loop's hoisted `nonvirtual_fast_door_on` instead of
// re-reading `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR`. No behaviour change:
// every line is the same in every mode and under every switch. Each shape
// runs cold first, then in a loop, then cold again:
//
//   virtual   an empty `invokevirtual`, a trivial getter, a plain body
//   private   a private call in the same class and one from a nestmate (the
//             virtual door's hand-off to the non-virtual door body)
//   iface     an empty `invokeinterface` and a plain one
//   ctor      a field-store constructor (`new Point(i, j)`) and a default one
//   frames    recursion to a fixed depth (retired-slot reuse on the return
//             path) and to a NEW depth each time (the first-call install)
//   sync      a synchronized instance method (the door's monitor)
//
// Positive controls (debug `cratonvm`, `--nojit` and default, both modes;
// add `CRATONVM_DBG_FIELD_SITE=1` and read the FINAL `[site-cache]` line):
// (measured on a Windows debug build, wave 61, `--nojit`):
// * default: `install-same: 1871758`, `door: static hit=97683`,
//   `special hit=1235255`, `install: reuse=12417 emplace=4557 byvalue=0`.
// * `CRATONVM_JIT_NO_FRAME_SLOT_REUSE=1`: `install-same: 0` and
//   `install: reuse=0` (`emplace=16974`); stdout unchanged.
// * `CRATONVM_JIT_NO_FRAME_EMPLACE=1`: `install: reuse=0 emplace=0
//   byvalue=16974`; stdout unchanged.
// * `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR=1`: `door: static hit=0 miss=0
//   special hit=0` (the private hand-off and both non-virtual doors
//   decline), `install: reuse=` in the millions; stdout unchanged.
// * `CRATONVM_JIT_VIRTUAL_TIERUP=0` (JIT on): stdout unchanged.
// * `CRATONVM_JIT_NO_DOOR_RECEIVER_RECORD=1`, `CRATONVM_JIT_NO_DOOR_RECV_MEMO=1`,
//   `CRATONVM_LOADER_NO_ANN_PROXY_LATCH=1`: stdout unchanged.
//
// Expected (HotSpot 25.0.3, `java L7W61DoorMemoGates`, and `-Xint`):
//   cold virtual 0 7 15
//   cold private 14 21
//   cold iface 0 9
//   cold ctor 3 7 1
//   cold frames 55 6 1
//   cold sync 4
//   loop virtual 0 2100000 2250000
//   loop private 4200000 6300000
//   loop iface 0 2700000
//   loop ctor 900000 2100000 300000
//   loop frames 16500 8860 300
//   loop sync 1200000
//   cold virtual 0 7 15
//   cold frames 55 6 1
public class L7W61DoorMemoGates {
    static class Box {
        int v = 7;
        int count;

        void empty() {}

        int get() {
            return v;
        }

        int plus(int x) {
            return v + x;
        }

        private int twice() {
            return v * 2;
        }

        int callTwice() {
            return twice();
        }

        synchronized void bump() {
            count++;
        }
    }

    static final class Peer {
        int viaNest(L7W61DoorMemoGates outer) {
            return outer.thrice();
        }
    }

    interface Listener {
        void on(int x);

        int map(int x);
    }

    static final class Quiet implements Listener {
        public void on(int x) {}

        public int map(int x) {
            return x + 9;
        }
    }

    static final class Point {
        final int a;
        final int b;

        Point(int a, int b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class Plain {}

    int w = 7;

    private int thrice() {
        return w * 3;
    }

    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    static int depth(int n) {
        return n == 0 ? 0 : 1 + depth(n - 1);
    }

    static int one(int n) {
        return n >= 0 ? 1 : 0;
    }

    static long[] virtualRow(Box b, int n) {
        long e = 0;
        long g = 0;
        long p = 0;
        for (int i = 0; i < n; i++) {
            b.empty();
            g += b.get();
            p += b.plus(i & 1);
        }
        return new long[] {e, g, p};
    }

    static long[] privateRow(Box b, Peer peer, L7W61DoorMemoGates outer, int n) {
        long s = 0;
        long t = 0;
        for (int i = 0; i < n; i++) {
            s += b.callTwice();
            t += peer.viaNest(outer);
        }
        return new long[] {s, t};
    }

    static long[] ifaceRow(Listener l, int n) {
        long e = 0;
        long m = 0;
        for (int i = 0; i < n; i++) {
            l.on(i);
            m += l.map(0);
        }
        return new long[] {e, m};
    }

    static long[] ctorRow(int n) {
        long a = 0;
        long b = 0;
        long c = 0;
        for (int i = 0; i < n; i++) {
            Point p = new Point(3, 7);
            a += p.a;
            b += p.b;
            if (new Plain() != null) {
                c++;
            }
        }
        return new long[] {a, b, c};
    }

    static long[] framesRow(int n) {
        long f = 0;
        long d = 0;
        long o = 0;
        for (int i = 0; i < n; i++) {
            f += fib(10) / 55 * 55 == 55 ? 55 : 0;
            d += depth(i % 61) == i % 61 ? i % 61 : -1000000;
            o += one(i);
        }
        return new long[] {f, d, o};
    }

    static long syncRow(Box b, int n) {
        int before = b.count;
        for (int i = 0; i < n; i++) {
            b.bump();
        }
        return b.count - before;
    }

    static String join(String name, long[] xs) {
        StringBuilder sb = new StringBuilder(name);
        for (long x : xs) {
            sb.append(' ').append(x);
        }
        return sb.toString();
    }

    static void coldVirtual() {
        Box b = new Box();
        b.empty();
        System.out.println("cold virtual 0 " + b.get() + " " + b.plus(8));
    }

    static void coldFrames() {
        System.out.println("cold frames " + fib(10) + " " + depth(6) + " " + one(3));
    }

    public static void main(String[] args) {
        coldVirtual();
        Box b0 = new Box();
        L7W61DoorMemoGates outer = new L7W61DoorMemoGates();
        System.out.println("cold private " + b0.callTwice() + " " + new Peer().viaNest(outer));
        Listener q = new Quiet();
        q.on(1);
        System.out.println("cold iface 0 " + q.map(0));
        Point p0 = new Point(3, 7);
        System.out.println("cold ctor " + p0.a + " " + p0.b + " " + (new Plain() != null ? 1 : 0));
        coldFrames();
        Box s0 = new Box();
        for (int i = 0; i < 4; i++) {
            s0.bump();
        }
        System.out.println("cold sync " + s0.count);

        final int n = 300_000;
        System.out.println(join("loop virtual", virtualRow(new Box(), n)));
        System.out.println(join("loop private", privateRow(new Box(), new Peer(), outer, n)));
        System.out.println(join("loop iface", ifaceRow(q, n)));
        System.out.println(join("loop ctor", ctorRow(n)));
        System.out.println(join("loop frames", framesRow(300)));
        System.out.println("loop sync " + syncRow(new Box(), n * 4));

        coldVirtual();
        coldFrames();
    }
}
