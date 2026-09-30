// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L2: item 3 of
// `docs/internal/fixed-bugs/interpreter-L2-compile-door-review-items-left-open-RETIRED-20261010.md`.
// Under `CRATONVM_BG_COMPILE=0` the eager first-call door in `execute`
// compiles a method at its FIRST call, and it refused the whole compile when
// a `getstatic` / `putstatic` owner could not be resolved without loading --
// the common case at a first call, when nothing has loaded the field's class
// yet -- and then SEALED the method (`early-backend-bail` into the name-keyed
// `jit_skip_set`), so the method was kept out of that door and out of
// `execute`'s cache consult for the life of the VM. Since wave 39 the door
// defers such a miss a few times per method (the interpreted call it falls
// back to resolves the field), then seals as before.
//
// Performance and diagnostics only: the output is the same either way.
// `read()` is reached through `execute` by reflection (`Method.invoke`), the
// route the eager door serves.
//
// Run: javac -d out L2W39EagerDoorStaticFieldMiss.java
//      CRATONVM_BG_COMPILE=0 cratonvm -cp out L2W39EagerDoorStaticFieldMiss
//
// Expected HotSpot 25 output (every mode, and CratonVM's in every mode):
//   reflective sum=1050000
//   direct sum=1050000
//
// Positive control (CratonVM, `CRATONVM_BG_COMPILE=0 CRATONVM_DBG_JITC=1`,
// stderr): a line
//   [cratonvm-jitc] eager-first-call resolver miss L2W39EagerDoorStaticFieldMiss.read()I pc=0 static cp=...: deferred
// and NO `early-backend-bail` seal of `L2W39EagerDoorStaticFieldMiss.read`
// after it (before wave 39 the miss sealed it on the first call). Whether
// the first call misses depends on `resolve_field_ref` refusing a field of
// a class nothing has loaded; if no `resolver miss` line appears, the door
// resolved it (and the case never arises for this probe).
import java.lang.reflect.Method;

public class L2W39EagerDoorStaticFieldMiss {
    static final class Holder {
        static int value = 104;

        static {
            value += 1;
        }
    }

    public static int read() {
        return Holder.value;
    }

    public static void main(String[] args) throws Exception {
        Method m = L2W39EagerDoorStaticFieldMiss.class.getMethod("read");
        long sum = 0;
        for (int i = 0; i < 10_000; i++) {
            sum += (int) m.invoke(null);
        }
        System.out.println("reflective sum=" + sum);
        long direct = 0;
        for (int i = 0; i < 10_000; i++) {
            direct += read();
        }
        System.out.println("direct sum=" + direct);
    }
}
