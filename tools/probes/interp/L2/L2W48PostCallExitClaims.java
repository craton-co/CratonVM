// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L2: a withdrawn single-pass body leaves
// right after a call whose successor wave 47 refused a post-call exit site
// for (docs/internal/fixed-bugs/interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md,
// item 2 of "Progress (wave 40)"): a forward conditional branch, an array
// load and a static store. Since wave 48 the site records a CLAIM that is
// settled after the walk by what the successor's own arm filed at its bci
// (docs/internal/fixed-bugs/interpreter-L2-proposal-post-call-exit-maps-are-filed-by-the-successors-own-arm-FIXED-20261012.md;
// `jit/src/x64/op_invoke.rs::POST_CALL_EXIT_CLAIMS_ENABLED`), so none of the
// three is predicted away any more.
//
// Built like L2W46FieldReadAfterTheCall (read its header first): a compiled
// body that ELIDED an empty constructor is inside a call when that
// constructor is retransformed into one that counts (`made += 1`); right
// after the call comes the row's successor, then a `new` of the class.
//
//   branch    `if (parkOnB(idx) != 0) ...`: the successor is `ifeq`.
//   array     `ARR[parkOnA(idx)]`: the successor is `iaload`.
//   putstatic `sink = parkOnS(idx)`: the successor is `putstatic`.
//   sixty-fifth  64 real calls, then `int parked = parkOnM(idx)`: the park's
//             site is the compile's 65th (item 4 of the i24-L6 page;
//             `op_invoke.rs::POST_CALL_EXIT_SITES_WIDE_CAP`, 1024 sites).
// Each body is warmed by 100,000 calls with idx 0 (compiled at method entry),
// then called with idx 1, whose `parkOn?(1)` waits on a latch main releases
// after the retransform. `ctor-ran-after-the-call` says whether the `new`
// after the successor ran the retransformed constructor: `true` when the
// frame left for the interpreter before it (or ran interpreted).
//
// HotSpot 25 (agent, JIT on and -Xint alike; JDK 25.0.3 locally):
//     branch ctor-ran-after-the-call=true
//     array ctor-ran-after-the-call=true
//     putstatic ctor-ran-after-the-call=true
//     sixty-fifth ctor-ran-after-the-call=true
// Without the agent: `no agent`.
//
// CratonVM, expected (not run by the lane that wrote it):
// * `--nojit`: HotSpot's lines.
// * JIT on, `CRATONVM_C2_SUPERSEDE=0` (the single-pass method-entry body
//   stays): HotSpot's lines with `POST_CALL_EXIT_CLAIMS_ENABLED` and
//   `POST_CALL_EXIT_SITES_WIDE_CAP` on. With them off (wave 47), a row whose
//   body elided the constructor prints `false`: its post-call site is refused
//   (`post_call_exit_successor_admitted`; the sixty-fifth row: past the
//   64-site cap), and the successor and the `new` run compiled with the
//   constructor elided.
//   POSITIVE CONTROL, under
//   `CRATONVM_C2_SUPERSEDE=0 CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1`:
//     `post-call exit claim at bci=<n> of L2W48PostCallExitClaims.branchBody...: Own`
//   (and `arrayBody`, `storeBody`), then a
//     `post-call exit verdict #k: ...Body... named=true withdrawn=true verdict=0x..`
//   line (non-zero) for each. A body whose constructor was not elided is not
//   withdrawn and prints `true` on both builds: check the control before
//   reading a row.
// * JIT on, default settings: the body may be the optimizing tier's, whose
//   post-call sites are wave 38's; a line may then print `false` without
//   contradicting the above.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W48PostCallExitClaims$Agent
//     Can-Retransform-Classes: true
// containing L2W48PostCallExitClaims*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W48PostCallExitClaims
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W48PostCallExitClaims {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The branch row's class: an empty constructor until retransformed. */
    public static class MarkerB {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    /** The array row's class. */
    public static class MarkerA {
        static int made;

        static int count() {
            return made;
        }
    }

    /** The putstatic row's class. */
    public static class MarkerS {
        static int made;

        static int count() {
            return made;
        }
    }

    static final String PREFIX = "L2W48PostCallExitClaims$";

    /**
     * On a RETRANSFORM of a marker class (never at the initial load), rewrites
     * the empty constructor `aload_0; invokespecial Object.<init>; return`
     * into `aload_0; invokespecial Object.<init>; made += 1; return`.
     */
    static final class Rewrite implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !((PREFIX + "MarkerB").equals(className)
                            || (PREFIX + "MarkerA").equals(className)
                            || (PREFIX + "MarkerS").equals(className)
                            || (PREFIX + "MarkerM").equals(className))) {
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

    // ---- branch: the call's successor is `ifeq` ---------------------------

    static final CountDownLatch GO_B = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_B = {OPEN, GO_B};
    static volatile boolean branchIn;

    /** A real call (it has an exception table), as in the parent probes. */
    static int parkOnB(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                branchIn = true;
            }
            LATCHES_B[idx].await();
            return idx;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    static int branchBody(int idx) throws InterruptedException {
        int before = MarkerB.made;
        int seen = 0;
        if (parkOnB(idx) != 0) {
            seen = 0;
        }
        MarkerB m = new MarkerB();
        return MarkerB.made - before + seen;
    }

    // ---- array: the call's successor is `iaload` --------------------------

    static final int[] ARR = {0, 0};
    static final CountDownLatch GO_A = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_A = {OPEN, GO_A};
    static volatile boolean arrayIn;

    static int parkOnA(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                arrayIn = true;
            }
            LATCHES_A[idx].await();
            return idx;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    static int arrayBody(int idx) throws InterruptedException {
        int before = MarkerA.made;
        int[] arr = ARR;
        int seen = arr[parkOnA(idx)];
        MarkerA m = new MarkerA();
        return MarkerA.made - before + seen;
    }

    // ---- putstatic: the call's successor is `putstatic` -------------------

    static final CountDownLatch GO_S = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_S = {OPEN, GO_S};
    static volatile boolean storeIn;
    /** Not a constant: `putstatic` writes it. */
    static int sink;

    static int parkOnS(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                storeIn = true;
            }
            LATCHES_S[idx].await();
            return idx;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    static int storeBody(int idx) throws InterruptedException {
        int before = MarkerS.made;
        sink = parkOnS(idx);
        MarkerS m = new MarkerS();
        return MarkerS.made - before;
    }

    // ---- sixty-fifth: the park is the compile's 65th real call ------------

    /** The sixty-fifth row's class. */
    public static class MarkerM {
        static int made;

        static int count() {
            return made;
        }
    }

    static final CountDownLatch GO_M = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_M = {OPEN, GO_M};
    static volatile boolean manyIn;

    /** A real call that is not spliced (it has an exception table). */
    static int real(int x) {
        try {
            return x + 1;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    static int parkOnM(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                manyIn = true;
            }
            LATCHES_M[idx].await();
            return idx;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** Sixty-four real calls, then the park: its site is the 65th. */
    static int manyBody(int idx) throws InterruptedException {
        int before = MarkerM.made;
        int s = 0;
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s); s = real(s);
        int parked = parkOnM(idx);
        MarkerM m = new MarkerM();
        return MarkerM.made - before + (s - 64) + (parked - idx);
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
        System.out.println("branch ctor-ran-after-the-call="
                + row(i, L2W48PostCallExitClaims::branchBody, MarkerB.class, GO_B, () -> branchIn));
        System.out.println("array ctor-ran-after-the-call="
                + row(i, L2W48PostCallExitClaims::arrayBody, MarkerA.class, GO_A, () -> arrayIn));
        System.out.println("putstatic ctor-ran-after-the-call="
                + row(i, L2W48PostCallExitClaims::storeBody, MarkerS.class, GO_S, () -> storeIn));
        System.out.println("sixty-fifth ctor-ran-after-the-call="
                + row(i, L2W48PostCallExitClaims::manyBody, MarkerM.class, GO_M, () -> manyIn));
    }
}
