// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L2: a withdrawn single-pass METHOD-ENTRY
// body leaves right after a call whose successor is a field READ
// (`getstatic`, `getfield`), before the read runs
// (docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md,
// item 2 of "Progress (wave 40)"; `op_invoke.rs::POST_CALL_EXIT_FIELD_READ_SUCCESSORS`).
// Built like L2W44CpHelperExitAfterTheCall (read its header first): a
// compiled body that ELIDED an empty constructor is inside a call when that
// constructor is retransformed into one that counts (`made += 1`); right
// after the call it reads a field, then constructs one.
//
//   getstatic  the call's successor is `getstatic seenG:I`.
//   getfield   the call returns a Holder and its successor is `getfield value:I`.
// Each body is warmed by 100,000 calls with idx 0 (compiled at method entry),
// then called with idx 1, whose `parkOn?(1)` waits on a latch main releases
// after the retransform. `ctor-ran-after-the-call` says whether the `new`
// after the read ran the retransformed constructor: `true` when the frame
// left for the interpreter before it (or ran interpreted).
//
// HotSpot 25 (agent, JIT on and -Xint alike; JDK 25.0.3 locally):
//     getstatic ctor-ran-after-the-call=true
//     getfield ctor-ran-after-the-call=true
// Without the agent: `no agent`.
//
// CratonVM, expected (not run by the lane that wrote it):
// * `--nojit`: HotSpot's lines.
// * JIT on, `CRATONVM_C2_SUPERSEDE=0` (the single-pass method-entry body
//   stays): HotSpot's lines with `POST_CALL_EXIT_FIELD_READ_SUCCESSORS` on.
//   On the base (55834015b), and with the switch off, a row prints `false`
//   there when its body elided the constructor: the post-call site is
//   refused (`post_call_exit_successor_admitted`), and the read and the rest
//   of the body run compiled to its return with the constructor elided.
//   POSITIVE CONTROL, under
//   `CRATONVM_C2_SUPERSEDE=0 CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1`:
//     `post-call exit verdict #k: L2W46FieldReadAfterTheCall.staticBody... named=true withdrawn=true verdict=0x..` (non-zero)
//   and the same for `fieldBody`; neither line names either body on the
//   base (a body whose constructor was not elided is not withdrawn, and
//   prints `true` on both builds: check the control before reading a row).
// * JIT on, default settings: the body may be the optimizing tier's, whose
//   post-call sites are wave 38's; either line may then print `false`
//   without contradicting the above.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W46FieldReadAfterTheCall$Agent
//     Can-Retransform-Classes: true
// containing L2W46FieldReadAfterTheCall*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W46FieldReadAfterTheCall
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W46FieldReadAfterTheCall {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The getstatic row's class: an empty constructor until retransformed. */
    public static class MarkerG {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    /** The getfield row's class: an empty constructor until retransformed. */
    public static class MarkerF {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    static final String PREFIX = "L2W46FieldReadAfterTheCall$";

    /**
     * On a RETRANSFORM of MarkerG / MarkerF (never at the initial load),
     * rewrites the empty constructor `aload_0; invokespecial Object.<init>;
     * return` into `aload_0; invokespecial Object.<init>; made += 1; return`.
     */
    static final class Rewrite implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !((PREFIX + "MarkerG").equals(className)
                            || (PREFIX + "MarkerF").equals(className))) {
                return null;
            }
            try {
                return countingConstructor(bytes, className);
            } catch (RuntimeException e) {
                e.printStackTrace();
                return null;
            }
        }
    }

    /** Offset past the constant-pool entry with tag `tag` at `at` (tag byte included). */
    static int skipEntry(ByteBuffer b, int at, int tag) {
        switch (tag) {
            case 1: return at + 3 + (b.getShort(at + 1) & 0xFFFF);
            case 3: case 4: case 9: case 10: case 11: case 12: case 17: case 18: return at + 5;
            case 5: case 6: return at + 9;
            case 7: case 8: case 16: case 19: case 20: return at + 3;
            case 15: return at + 4;
            default: throw new IllegalStateException("constant tag " + tag);
        }
    }

    static byte[] countingConstructor(byte[] bytes, String className) {
        ByteBuffer b = ByteBuffer.wrap(bytes);
        int count = b.getShort(8) & 0xFFFF;
        int[] offset = new int[count];
        int at = 10;
        for (int i = 1; i < count; i++) {
            offset[i] = at;
            int tag = bytes[at];
            at = skipEntry(b, at, tag);
            if (tag == 5 || tag == 6) {
                i++;
            }
        }
        int field = 0;
        for (int i = 1; i < count; i++) {
            int o = offset[i];
            if (o == 0 || bytes[o] != 9) {
                continue;
            }
            int cls = b.getShort(o + 1) & 0xFFFF;
            int nat = b.getShort(o + 3) & 0xFFFF;
            String owner = utf8(b, offset, b.getShort(offset[cls] + 1) & 0xFFFF);
            String name = utf8(b, offset, b.getShort(offset[nat] + 1) & 0xFFFF);
            String desc = utf8(b, offset, b.getShort(offset[nat] + 3) & 0xFFFF);
            if (owner.equals(className) && name.equals("made") && desc.equals("I")) {
                field = i;
            }
        }
        if (field == 0) {
            throw new IllegalStateException("no Fieldref made:I in " + className);
        }
        at += 6;
        int interfaces = b.getShort(at) & 0xFFFF;
        at += 2 + 2 * interfaces;
        int fields = b.getShort(at) & 0xFFFF;
        at += 2;
        for (int f = 0; f < fields; f++) {
            at = skipMember(b, at);
        }
        int methods = b.getShort(at) & 0xFFFF;
        at += 2;
        for (int m = 0; m < methods; m++) {
            String name = utf8(b, offset, b.getShort(at + 2) & 0xFFFF);
            String desc = utf8(b, offset, b.getShort(at + 4) & 0xFFFF);
            int attrs = b.getShort(at + 6) & 0xFFFF;
            int a = at + 8;
            for (int k = 0; k < attrs; k++) {
                int len = b.getInt(a + 2);
                String attr = utf8(b, offset, b.getShort(a) & 0xFFFF);
                if (name.equals("<init>") && desc.equals("()V") && attr.equals("Code")) {
                    int maxLocals = b.getShort(a + 8) & 0xFFFF;
                    int codeLen = b.getInt(a + 10);
                    int code = a + 14;
                    if (codeLen != 5 || bytes[code] != 0x2a || bytes[code + 1] != (byte) 0xb7
                            || bytes[code + 4] != (byte) 0xb1) {
                        return null;
                    }
                    byte hi = (byte) (field >> 8);
                    byte lo = (byte) field;
                    byte[] body = {
                        0x2a, (byte) 0xb7, bytes[code + 2], bytes[code + 3],
                        (byte) 0xb2, hi, lo,
                        0x04, 0x60,
                        (byte) 0xb3, hi, lo,
                        (byte) 0xb1,
                    };
                    ByteArrayOutputStream out = new ByteArrayOutputStream();
                    out.write(bytes, 0, a + 2);
                    ByteBuffer attrBytes = ByteBuffer.allocate(4 + 2 + 2 + 4 + body.length + 2 + 2);
                    attrBytes.putInt(2 + 2 + 4 + body.length + 2 + 2);
                    attrBytes.putShort((short) 2);
                    attrBytes.putShort((short) maxLocals);
                    attrBytes.putInt(body.length);
                    attrBytes.put(body);
                    attrBytes.putShort((short) 0);
                    attrBytes.putShort((short) 0);
                    out.write(attrBytes.array(), 0, attrBytes.position());
                    int end = a + 6 + len;
                    out.write(bytes, end, bytes.length - end);
                    return out.toByteArray();
                }
                a += 6 + len;
            }
            at = a;
        }
        throw new IllegalStateException("no <init>()V in " + className);
    }

    static int skipMember(ByteBuffer b, int at) {
        int attrs = b.getShort(at + 6) & 0xFFFF;
        int a = at + 8;
        for (int k = 0; k < attrs; k++) {
            a += 6 + b.getInt(a + 2);
        }
        return a;
    }

    static String utf8(ByteBuffer b, int[] offset, int index) {
        int o = offset[index];
        int len = b.getShort(o + 1) & 0xFFFF;
        return new String(b.array(), o + 3, len, StandardCharsets.UTF_8);
    }

    static final CountDownLatch OPEN = new CountDownLatch(0);

    // ---- getstatic ------------------------------------------------------

    static final CountDownLatch GO_G = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_G = {OPEN, GO_G};
    static volatile boolean staticIn;
    /** Not a constant: `getstatic` reads it. */
    static int seenG;

    /** A real call (it has an exception table), as in the parent probes. */
    static void parkOnG(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                staticIn = true;
            }
            LATCHES_G[idx].await();
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** The call's successor is a `getstatic`. */
    static int staticBody(int idx) throws InterruptedException {
        int before = MarkerG.made;
        parkOnG(idx);
        int seen = seenG;
        MarkerG m = new MarkerG();
        return MarkerG.made - before + seen;
    }

    // ---- getfield -------------------------------------------------------

    /** The object the getfield row's call returns. */
    public static class Holder {
        int value;
    }

    static final Holder HOLDER = new Holder();
    static final CountDownLatch GO_F = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_F = {OPEN, GO_F};
    static volatile boolean fieldIn;

    static Holder parkOnF(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                fieldIn = true;
            }
            LATCHES_F[idx].await();
            return HOLDER;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** The call's successor is a `getfield` of the object it returned. */
    static int fieldBody(int idx) throws InterruptedException {
        int before = MarkerF.made;
        int seen = parkOnF(idx).value;
        MarkerF m = new MarkerF();
        return MarkerF.made - before + seen;
    }

    interface Body {
        int run(int idx) throws InterruptedException;
    }

    static boolean row(Instrumentation i, Body body, Class<?> marker, CountDownLatch go,
            java.util.function.BooleanSupplier in) throws Exception {
        int[] out = {-1};
        Thread worker = new Thread(() -> {
            try {
                for (int k = 0; k < 100_000; k++) {
                    body.run(0);
                }
                out[0] = body.run(1);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!in.getAsBoolean()) {
            Thread.sleep(1);
        }
        // Parked in `go.await()`, a call made from inside the compiled body.
        Thread.sleep(50);
        i.retransformClasses(marker);
        go.countDown();
        worker.join();
        return out[0] == 1;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Rewrite(), true);
        System.out.println("getstatic ctor-ran-after-the-call="
                + row(i, L2W46FieldReadAfterTheCall::staticBody, MarkerG.class, GO_G, () -> staticIn));
        System.out.println("getfield ctor-ran-after-the-call="
                + row(i, L2W46FieldReadAfterTheCall::fieldBody, MarkerF.class, GO_F, () -> fieldIn));
    }
}
