// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 94, lane L7
// (i92-L7-selection-and-callback-memos-need-a-hierarchy-edge-epoch): the
// JVMS 5.4.6 selection memo (`resolve::selection::SELECT_MEMO`, and the
// hierarchy-is-real memo beside it) retiring on every class definition. Its
// entries are stamped with the store's class-definition epoch and the
// process-wide `class_origin_epoch`, and BOTH move on every define, so a
// define-per-iteration workload re-walks every selection it asks again.
// `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH` (default off) stamps them with
// the store's hierarchy-edge epoch and `class_origin_in_place_epoch`
// instead, which a definition of an unrelated class does not move.
//
// The consumer: an `invokeinterface` site whose receivers inherit the method
// (receiver class != declaring class) asks selection on each per-thread
// interface-select-site miss (`dispatch_virtual`, `iface_select_sites`), and
// a site rotating past the poly cache asks it on the slow path; both are
// retired by every define either way, so the memo is what is measured.
//
// Rows (ROUNDS = 400 rounds each; one op = one round):
//   define-only   define one fresh class through the custom loader
//   define-calls  the same, then CALLS = 64 interface calls rotating over 16
//                 receiver classes that all inherit `f` from `Base`
//   calls-only    the 64 calls with no define (the floor)
// The memo's share is (define-calls - define-only) - calls-only.
//
// Run (--nojit: pure interpretation; each arm with the switch off and on):
//   javac -d out L7W94SelectMemoChurnBench.java
//   java -cp out L7W94SelectMemoChurnBench                                  (HotSpot reference)
//   cratonvm --jdk-only --nojit -cp out L7W94SelectMemoChurnBench
//   CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH=1 cratonvm --jdk-only --nojit -cp out L7W94SelectMemoChurnBench
// Interleave the arms, several rounds, medians. Census: CRATONVM_DBG_FIELD_SITE=1
// (the site census; its IFACE_SELECT_MISS row counts the per-define misses
// that reach selection).
//
// Output: "checksum <row> <n>" on stdout (deterministic); the timing line
// "row <row> min <ns> med <ns> ns/op" on stderr.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// checksum define-only 400
// checksum define-calls 25600
// checksum calls-only 25600
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.Arrays;

public class L7W94SelectMemoChurnBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final int ROUNDS = 400;
    static final int WARM = 40;
    static final int CALLS = 64;
    static int serial;

    interface I {
        int f();
    }

    static class Base implements I {
        public int f() {
            return 1;
        }
    }

    static final class S0 extends Base {}
    static final class S1 extends Base {}
    static final class S2 extends Base {}
    static final class S3 extends Base {}
    static final class S4 extends Base {}
    static final class S5 extends Base {}
    static final class S6 extends Base {}
    static final class S7 extends Base {}
    static final class S8 extends Base {}
    static final class S9 extends Base {}
    static final class S10 extends Base {}
    static final class S11 extends Base {}
    static final class S12 extends Base {}
    static final class S13 extends Base {}
    static final class S14 extends Base {}
    static final class S15 extends Base {}

    static final I[] RECEIVERS = {
        new S0(), new S1(), new S2(), new S3(), new S4(), new S5(), new S6(), new S7(),
        new S8(), new S9(), new S10(), new S11(), new S12(), new S13(), new S14(), new S15(),
    };

    interface Row {
        int op(Definer loader) throws Exception;
    }

    static final class Definer extends ClassLoader {
        Definer() {
            super(L7W94SelectMemoChurnBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    // One interface site, 16 receiver classes in rotation, each call 1.
    static int calls() {
        I[] r = RECEIVERS;
        int s = 0;
        for (int i = 0; i < CALLS; i++) {
            s += r[i & 15].f();
        }
        return s;
    }

    static int defineOne(Definer loader) {
        String name = "L7W94SelChurn" + (serial++);
        Class<?> k = loader.define(name, build(name));
        return k.getName().equals(name) ? 1 : 0;
    }

    static void time(String row, Definer loader, Row r) throws Exception {
        for (int i = 0; i < WARM; i++) {
            r.op(loader);
        }
        long[] ns = new long[ROUNDS];
        long checksum = 0;
        for (int i = 0; i < ROUNDS; i++) {
            long t0 = System.nanoTime();
            checksum += r.op(loader);
            ns[i] = System.nanoTime() - t0;
        }
        Arrays.sort(ns);
        System.out.println("checksum " + row + " " + checksum);
        System.err.println("row " + row + " min " + ns[0] + " med " + ns[ROUNDS / 2] + " ns/op");
    }

    public static void main(String[] args) throws Exception {
        Definer loader = new Definer();
        time("define-only", loader, L7W94SelectMemoChurnBench::defineOne);
        time("define-calls", loader, l -> {
            defineOne(l);
            return calls();
        });
        time("calls-only", loader, l -> calls());
    }
}
