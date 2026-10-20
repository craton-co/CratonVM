// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: INDY21-2, the compiled class-initialization
// barrier for `getstatic` (`CRATONVM_JIT_IR_CLINIT_BARRIER`:
// `Lowerer::emit_clinit_barrier_getstatic`, the per-class init-owner cell
// `jit_bridge::clinit_owner_cell_for`, cleared in `finalize_class_init`) and
// the helper's own-clinit fast path (`CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST`).
//
// Risk targeted: while a class is being initialized, ONLY the initializing
// thread may read its statics inline (JVMS 5.5 step 3: a recursive request
// returns at once); every other thread must block until `<clinit>` ends, and
// after a FAILED `<clinit>` every read must raise NoClassDefFoundError.
//   * a hot reader method (compiled while the holder is uninitialized,
//     called 200k times from the holder's own `<clinit>`) reading int, long,
//     double, byte, boolean and reference statics that the `<clinit>` keeps
//     changing -- each read must see the latest write (no stale inline value,
//     no folded default);
//   * a second thread that reads the same statics during the `<clinit>`:
//     it must see only the FINAL values (it blocks);
//   * a holder whose `<clinit>` throws after warming the reader: the next
//     reads from the main thread and from another thread raise NCDFE
//     (the owner cell must be cleared, the inline path must not serve);
//   * a reader of a class B from inside class A's `<clinit>` (B not being
//     initialized by anyone: the barrier must fall to the helper, which
//     initializes B).
//
// Run:
//   javac -d out R21FzIndyClinit1.java
//   java -Xmx256m -cp out R21FzIndyClinit1                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzIndyClinit1
//   CRATONVM_JIT_IR_CLINIT_BARRIER=1 cratonvm -Xmx256m -cp out R21FzIndyClinit1
//   CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST=1 cratonvm ...; both together
//   CRATONVM_JIT_STATIC_INIT_RACE_SERVE=0 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzIndyClinit1 {
    static final int CALLS = 200_000;

    static final class Holder {
        static int i;
        static long l;
        static double d;
        static byte b;
        static boolean z;
        static String s = "init";
        static long readerSum;
        static {
            Thread other = new Thread(() -> otherSaw = R21FzIndyClinit1.readAll());
            OTHER = other;
            for (int k = 0; k < CALLS; k++) {
                i = k;
                l = (long) k << 33;
                d = k * 0.5;
                b = (byte) k;
                z = (k & 1) == 1;
                if ((k & 4095) == 0) {
                    s = "s" + k;
                }
                long got = R21FzIndyClinit1.readerHash();
                long want = R21FzIndyClinit1.expectHash(k);
                if (got != want) {
                    readerSum += 1_000_000_000L; // a stale read
                } else {
                    readerSum += 1;
                }
                if (k == CALLS / 2) {
                    other.start(); // it must block in readAll until <clinit> ends
                }
            }
            i = -1;
            s = "final";
            try {
                other.join(50);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        }
    }

    static volatile String otherSaw = "unset";
    static Thread OTHER;

    static long readerHash() {
        long h = Holder.i;
        h = h * 31 + Holder.l;
        h = h * 31 + (long) (Holder.d * 2);
        h = h * 31 + Holder.b;
        h = h * 31 + (Holder.z ? 1 : 0);
        h = h * 31 + Holder.s.length();
        return h;
    }

    static long expectHash(int k) {
        long h = k;
        h = h * 31 + ((long) k << 33);
        h = h * 31 + (long) (k * 0.5 * 2);
        h = h * 31 + (byte) k;
        h = h * 31 + (((k & 1) == 1) ? 1 : 0);
        String s = "s" + (k & ~4095);
        h = h * 31 + s.length();
        return h;
    }

    static String readAll() {
        return Holder.i + "," + Holder.l + "," + Holder.d + "," + Holder.b + "," + Holder.z + "," + Holder.s;
    }

    static int failCalls;

    static final class Failing {
        static int x;

        static {
            for (int k = 0; k < CALLS; k++) {
                x = k;
                failCalls += R21FzIndyClinit1.readFailing() == k ? 1 : 0;
            }
            if (failCalls > 0) {
                throw new IllegalStateException("Failing <clinit> fails after " + failCalls);
            }
        }
    }

    static int readFailing() {
        return Failing.x;
    }

    static final class Other {
        static int y = 41;

        static {
            y++;
        }
    }

    static final class Outer {
        static int seen;

        static {
            for (int k = 0; k < 100_000; k++) {
                seen += R21FzIndyClinit1.readOther();
            }
        }
    }

    static int readOther() {
        return Other.y;
    }

    static String exc(Throwable t) {
        String m = t.getClass().getName() + ": " + t.getMessage();
        if (t.getCause() != null) {
            m += " <- " + t.getCause().getClass().getName() + ": " + t.getCause().getMessage();
        }
        return m;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("holder " + readAll() + " readerSum=" + Holder.readerSum);
        OTHER.join();
        System.out.println("other " + otherSaw);
        for (int k = 0; k < 3; k++) {
            try {
                System.out.println("failing " + readFailing());
            } catch (Throwable t) {
                System.out.println("failing " + exc(t));
            }
        }
        String[] fromThread = new String[1];
        Thread t = new Thread(() -> {
            try {
                fromThread[0] = "thread " + readFailing();
            } catch (Throwable e) {
                fromThread[0] = "thread " + exc(e);
            }
        });
        t.start();
        t.join();
        System.out.println(fromThread[0]);
        System.out.println("failCalls " + failCalls);
        System.out.println("outer " + Outer.seen + " other " + Other.y);
        long h = 0;
        for (int k = 0; k < CALLS; k++) {
            h += readerHash() + readOther();
        }
        System.out.println("after " + h);
    }
}
