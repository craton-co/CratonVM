// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 16, lane finalize-reinit: residual 4R of
// docs/known-issues/jit/r21-review-w10-findings-20261019.md, case (a).
// A creator keeps the `uninitialized` reference of `new Retry` in a local, catches the
// constructor's IllegalArgumentException (thrown before `super(..)` on the first try) and
// runs `invokespecial Retry.<init>(I)V` again on the SAME object, which then completes.
// HotSpot (RegisterFinalizersAtInit) registers the object when the retry's Object.<init>
// returns, so every object is finalized exactly once.
//
// Plain javac never emits this creator (it never stores an uninitialized reference in a
// local), so the probe hand-assembles it: class `R21FinalizeRetryInitMaker`, version 52
// with a StackMapTable, defined through Lookup.defineClass in this package. Its static
// `make(I)Ljava/lang/Object;` is
//      0 new Retry; 3 astore_1; 4 aload_1; 5 iload_0; 6 invokespecial Retry.<init>(I)V;
//      9 aload_1; 10 areturn;
//     11 pop; 12 aload_1; 13 iconst_m1; 14 invokespecial Retry.<init>(I)V; 17 aload_1; 18 areturn
// with the row [4, 9) -> 11 catching IllegalArgumentException; the handler's frame keeps
// `uninitialized(0)` in local 1.
//
// Wave 17: VERIFIED CODE CANNOT EXPRESS CASE (a). HotSpot checks a handler's frame against
// the frame AFTER each covered instruction too (`verify_exception_handler_targets`), and after
// the `invokespecial` at 6 local 1 is `Retry`, not `uninitialized(0)`:
//   VerifyError: Stack map does not match the one at exception handler 11 ... Type
//   'R21FinalizeRetryInit$Retry' (current frame, locals[1]) is not assignable to
//   uninitialized 0 (stack map, locals[1])
// CratonVM's verifier makes the same post-instruction check (`check_handler_frames_at` after
// `replace_vtype_in_frame`), so both VMs refuse the creator. The probe prints the refusal's
// class. The retry rows run only when the creator is defined unverified, which HotSpot allows
// with -XX:-BytecodeVerificationRemote (where JDK 25 still accepts that flag). Case (a) is
// therefore reachable only with verification off, as is case (b) (an unverified class
// re-running <init> on an object already registered); both are covered by the unit tests.
//
// Switch: CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN (default ON). It changes only the
// unverified arm.
//
// Run:
//   javac -d out R21FinalizeRetryInit.java
//   java -Xmx256m -cp out R21FinalizeRetryInit                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FinalizeRetryInit             # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21FinalizeRetryInit
//   cratonvm --nojit -Xmx256m -cp out R21FinalizeRetryInit
//
// HotSpot expectation (every arm, both modes, verification on):
//   fin noretry 0
//   maker java.lang.VerifyError
// Unverified arm (java -XX:-BytecodeVerificationRemote ...; expected, not yet confirmed on
// HotSpot): `fin noretry 0`, `maker defined`, `fin retry 20000`, `fin retry hot 50000`.

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.concurrent.atomic.AtomicInteger;

public class R21FinalizeRetryInit {
    static final int N = 20_000;
    static final int HOT = 50_000;
    static final AtomicInteger FIN = new AtomicInteger();

    static int check(int v) {
        if (v >= 0) {
            throw new IllegalArgumentException("before super " + v);
        }
        return v;
    }

    static class Fin {
        final int v;

        Fin(int v) {
            this.v = v;
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FIN.incrementAndGet();
        }
    }

    /** Throws before super(..) for v >= 0 (the first try), completes for v < 0. */
    static class Retry extends Fin {
        Retry(int v) {
            super(check(v));
        }
    }

    static void collect() {
        System.gc();
        System.runFinalization();
        try {
            Thread.sleep(2);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** Collect until the count has not moved for 10 rounds (at least 20, at most 400). */
    static int settle() {
        int last = -1;
        int still = 0;
        for (int i = 0; i < 400; i++) {
            collect();
            int now = FIN.get();
            if (now == last) {
                still++;
            } else {
                still = 0;
                last = now;
            }
            if (i >= 20 && still >= 10) {
                break;
            }
        }
        return FIN.get();
    }

    /** The javac shape: the reference never reaches a local, nothing retries. */
    static int noRetry(int n) {
        int thrown = 0;
        for (int r = 0; r < n; r++) {
            try {
                new Retry(r);
            } catch (IllegalArgumentException e) {
                thrown++;
            }
        }
        return thrown;
    }

    static int retry(MethodHandle make, int n) throws Throwable {
        int made = 0;
        for (int r = 0; r < n; r++) {
            Object o = (Object) make.invokeExact(r);
            if (o instanceof Retry) {
                made++;
            }
        }
        return made;
    }

    /** The hand-assembled creator class (see the header). */
    static byte[] makerBytes() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(17); // constant pool count (entries 1..16)
        utf8(out, "R21FinalizeRetryInitMaker"); // 1
        classRef(out, 1); // 2
        utf8(out, "java/lang/Object"); // 3
        classRef(out, 3); // 4
        utf8(out, "R21FinalizeRetryInit$Retry"); // 5
        classRef(out, 5); // 6
        utf8(out, "<init>"); // 7
        utf8(out, "(I)V"); // 8
        out.writeByte(12); // 9 NameAndType
        out.writeShort(7);
        out.writeShort(8);
        out.writeByte(10); // 10 Methodref Retry.<init>(I)V
        out.writeShort(6);
        out.writeShort(9);
        utf8(out, "java/lang/IllegalArgumentException"); // 11
        classRef(out, 11); // 12
        utf8(out, "make"); // 13
        utf8(out, "(I)Ljava/lang/Object;"); // 14
        utf8(out, "Code"); // 15
        utf8(out, "StackMapTable"); // 16
        out.writeShort(0x0021); // public super
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(1); // methods
        out.writeShort(0x0009); // public static
        out.writeShort(13);
        out.writeShort(14);
        out.writeShort(1); // attributes: Code

        byte[] code = {
            (byte) 0xbb, 0, 6, // 0 new Retry
            0x4c, // 3 astore_1
            0x2b, // 4 aload_1
            0x1a, // 5 iload_0
            (byte) 0xb7, 0, 10, // 6 invokespecial Retry.<init>(I)V
            0x2b, // 9 aload_1
            (byte) 0xb0, // 10 areturn
            0x57, // 11 pop (handler)
            0x2b, // 12 aload_1
            0x02, // 13 iconst_m1
            (byte) 0xb7, 0, 10, // 14 invokespecial Retry.<init>(I)V
            0x2b, // 17 aload_1
            (byte) 0xb0, // 18 areturn
        };
        ByteArrayOutputStream smtBytes = new ByteArrayOutputStream();
        DataOutputStream smt = new DataOutputStream(smtBytes);
        smt.writeShort(1); // one frame
        smt.writeByte(255); // full_frame
        smt.writeShort(11); // at offset 11
        smt.writeShort(2); // locals
        smt.writeByte(1); // Integer
        smt.writeByte(8); // Uninitialized
        smt.writeShort(0); // ... created by the `new` at 0
        smt.writeShort(1); // stack
        smt.writeByte(7); // Object
        smt.writeShort(12); // IllegalArgumentException
        smt.flush();

        ByteArrayOutputStream codeAttr = new ByteArrayOutputStream();
        DataOutputStream c = new DataOutputStream(codeAttr);
        c.writeShort(2); // max_stack
        c.writeShort(2); // max_locals
        c.writeInt(code.length);
        c.write(code);
        c.writeShort(1); // exception table
        c.writeShort(4);
        c.writeShort(9);
        c.writeShort(11);
        c.writeShort(12);
        c.writeShort(1); // attributes: StackMapTable
        c.writeShort(16);
        c.writeInt(smtBytes.size());
        c.write(smtBytes.toByteArray());
        c.flush();

        out.writeShort(15);
        out.writeInt(codeAttr.size());
        out.write(codeAttr.toByteArray());
        out.writeShort(0); // class attributes
        out.flush();
        return bytes.toByteArray();
    }

    static void utf8(DataOutputStream out, String s) throws IOException {
        out.writeByte(1);
        out.writeUTF(s);
    }

    static void classRef(DataOutputStream out, int nameIndex) throws IOException {
        out.writeByte(7);
        out.writeShort(nameIndex);
    }

    public static void main(String[] args) throws Throwable {
        settle();
        FIN.set(0);
        int thrown = noRetry(N);
        if (thrown != N) {
            throw new AssertionError("noretry thrown " + thrown);
        }
        System.out.println("fin noretry " + settle());

        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle make;
        try {
            Class<?> maker = lookup.defineClass(makerBytes());
            // Verification runs at link: force it (no instance is made).
            lookup.ensureInitialized(maker);
            make = lookup.findStatic(maker, "make", MethodType.methodType(Object.class, int.class));
        } catch (LinkageError e) {
            System.out.println("maker " + e.getClass().getName());
            return;
        }
        System.out.println("maker defined");

        settle();
        FIN.set(0);
        int made = retry(make, N);
        if (made != N) {
            throw new AssertionError("retry made " + made);
        }
        System.out.println("fin retry " + settle());

        FIN.set(0);
        made = retry(make, HOT);
        if (made != HOT) {
            throw new AssertionError("retry hot made " + made);
        }
        System.out.println("fin retry hot " + settle());
    }
}
