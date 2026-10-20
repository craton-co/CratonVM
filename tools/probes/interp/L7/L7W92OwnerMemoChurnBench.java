// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 92, lane L7: the static door's loader-split
// guard (`dispatch_static::cached_static_owner_stale`) re-validating after
// every class definition. Once a user-defined loader has defined a class,
// every interpreted `invokestatic` asks the guard, whose thread-local memo
// (`OWNER_RECHECK_MEMO`, and the owner stamp under
// `CRATONVM_INTERP_STATIC_OWNER_STAMP`) is keyed on the VM's
// class-definition epoch, so each define retires every site's verdict.
// `CRATONVM_INTERP_OWNER_MEMO_NAME_GENERATION` (default off) keys it on the
// class-NAME generation instead, which a definition of a new name does not
// move (i89-L4-method-finder-per-class-and-per-handle-cost, wave 91 item 3).
//
// Rows (ROUNDS = 400 rounds each; one op = one round):
//   define-only   define one fresh class through the custom loader
//   define-calls  the same, then CALLS = 64 iterations over 4 static sites
//   calls-only    the 64 x 4 calls with no define (the floor)
// The guard's share is (define-calls - define-only) - calls-only.
//
// Run (--nojit: the guard is on the interpreter's static door; each arm with
// the switch off and on, also with CRATONVM_INTERP_STATIC_OWNER_STAMP=1):
//   javac -d out L7W92OwnerMemoChurnBench.java
//   java -cp out L7W92OwnerMemoChurnBench                                   (HotSpot reference)
//   cratonvm --jdk-only --nojit -cp out L7W92OwnerMemoChurnBench
//   CRATONVM_INTERP_OWNER_MEMO_NAME_GENERATION=1 cratonvm --jdk-only --nojit -cp out L7W92OwnerMemoChurnBench
// Interleave the arms, several rounds, medians.
//
// Output: "checksum <row> <n>" on stdout (deterministic); the timing line
// "row <row> min <ns> med <ns> ns/op" on stderr.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// checksum define-only 400
// checksum define-calls 76800
// checksum calls-only 76800
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.Arrays;

public class L7W92OwnerMemoChurnBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final int ROUNDS = 400;
    static final int WARM = 40;
    static final int CALLS = 64;
    static int serial;

    interface Row {
        int op(Definer loader) throws Exception;
    }

    static final class Definer extends ClassLoader {
        Definer() {
            super(L7W92OwnerMemoChurnBench.class.getClassLoader());
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

    static int a(int x) {
        return x & 1;
    }

    static int b(int x) {
        return (x >> 1) & 1;
    }

    static int c(int x) {
        return (x >> 2) & 1;
    }

    static int d(int x) {
        return (x >> 3) & 1;
    }

    // Over i = 0..63 each of the four low bits is set 32 times (4 x 32 =
    // 128), plus 64 for the `1 +` term: 192 per round.
    static int calls() {
        int s = 0;
        for (int i = 0; i < CALLS; i++) {
            s += 1 + a(i);
            s += b(i);
            s += c(i);
            s += d(i);
        }
        return s;
    }

    static int defineOne(Definer loader) {
        String name = "L7W92Churn" + (serial++);
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
        time("define-only", loader, L7W92OwnerMemoChurnBench::defineOne);
        time("define-calls", loader, l -> {
            defineOne(l);
            return calls();
        });
        time("calls-only", loader, l -> calls());
    }
}
