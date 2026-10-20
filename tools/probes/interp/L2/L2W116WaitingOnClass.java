// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L2: the tiered manager's
// "waiting on a class" park (`jit/src/tiered.rs` `note_compile_waiting_on_class`),
// fed on x86-64 by `CRATONVM_JIT_BG_RESOLVER_MISS_WAITS_ON_CLASS` (default off)
// and on aarch64 by `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT` (page
// docs/known-issues/interpreter/i107-L2-aarch64-unloaded-field-owner-bail-lists-the-method-20261209.md).
//
// Hot methods whose cold branch names a class nobody has loaded (a static
// field, a static call, a `new`, a `checkcast`), warmed far past three
// tier-up offers BEFORE the cold branch first runs. Without the park, a
// background compile that declines at the resolver miss is charged to the
// method's three retries and the method stays interpreted for good; with it,
// the method compiles once the class is defined. Output is identical in every
// arm; the TIME rows and the census differ.
//
// Run (one binary; --jdk-only and --compatible; JIT on, and --nojit once):
//   javac -d out tools/probes/interp/L2/L2W116WaitingOnClass.java
//   java -cp out L2W116WaitingOnClass                                   (HotSpot reference)
//   cratonvm --jdk-only -cp out L2W116WaitingOnClass
//   CRATONVM_JIT_BG_RESOLVER_MISS_WAITS_ON_CLASS=1 cratonvm --jdk-only -cp out L2W116WaitingOnClass
// Census (first, to see whether this workload declines at a miss at all on
// x86-64, where most such sites are deferred instead):
//   CRATONVM_DBG_JITC=1 [switch] cratonvm --jdk-only -cp out L2W116WaitingOnClass 2>&1 \
//     | grep -E 'resolver-bail|compile-bail|tiered-enqueue L2W116'
// and `CRATONVM_DBG=jit-method-stats` for `hot_but_stuck_in_interpreter`.
// Expected with the switch: no `tier_fail=3` for the four hot methods, and
// their TIME rows after the cold call at compiled speed.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// static-field 822497400000 822497400000
// static-call 822497400000 822497400000
// new-object 822497600000 822497600000
// checkcast 822496200000 822496200000
// HOTSPOT_EXPECTED_END

public class L2W116WaitingOnClass {
    static final int CALLS = 200_000;

    static int viaField(int i, boolean cold) {
        int x = i * 31 + (i >>> 3);
        if (cold) {
            return x + L2W116ColdField.BASE;
        }
        return x;
    }

    static int viaCall(int i, boolean cold) {
        int x = i * 31 + (i >>> 3);
        if (cold) {
            return x + L2W116ColdCall.base();
        }
        return x;
    }

    static int viaNew(int i, boolean cold) {
        int x = i * 31 + (i >>> 3);
        if (cold) {
            return x + new L2W116ColdNew().base;
        }
        return x;
    }

    static int viaCast(Object o, int i, boolean cold) {
        int x = i * 31 + (i >>> 3);
        if (cold) {
            return x + ((L2W116ColdCast) o).base;
        }
        return x;
    }

    interface Op {
        int run(int i, boolean cold);
    }

    static long loop(Op op, boolean cold) {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            sum += op.run(i, cold);
        }
        return sum;
    }

    static void row(String name, Op op) throws Exception {
        for (int w = 0; w < 5; w++) {
            loop(op, false);
        }
        Thread.sleep(200);
        long first = loop(op, true);
        Thread.sleep(300);
        long t0 = System.nanoTime();
        long again = loop(op, true);
        long t = System.nanoTime() - t0;
        System.out.println("TIME " + name + " " + t / CALLS + " ns/call");
        System.out.println(name + " " + first + " " + again);
    }

    public static void main(String[] args) throws Exception {
        row("static-field", L2W116WaitingOnClass::viaField);
        row("static-call", L2W116WaitingOnClass::viaCall);
        row("new-object", L2W116WaitingOnClass::viaNew);
        Object cast = new L2W116ColdCast();
        row("checkcast", (i, cold) -> viaCast(cold ? cast : null, i, cold));
    }
}

class L2W116ColdField {
    static int BASE = 1_000_000 + 3;
}

class L2W116ColdCall {
    static int base() {
        return 1_000_000 + 3;
    }
}

class L2W116ColdNew {
    int base = 1_000_000 + 4;
}

class L2W116ColdCast {
    int base = 1_000_000 - 3;
}
