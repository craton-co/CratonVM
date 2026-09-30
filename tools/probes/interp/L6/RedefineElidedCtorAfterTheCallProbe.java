// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 28, lane L6: the POSITIVE CONTROL for the
// patchable post-call exit (wave 27,
// docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md;
// `jit/src/x64/op_invoke.rs::emit_post_call_exit_site`).
//
// Why RedefineSpliceAfterTheCallProbe could not be that control: the
// single-pass tier's OSR compiles splice NO callee (`jit_bridge.rs::
// compile_osr_body` hands the backend an empty `inline_sites` map), so under
// `CRATONVM_JIT_OSR_OPTIMIZING=0` its `Callee.value()` is a real call that
// reaches the new bytecode by itself, the loop's body copies nothing of
// `Callee`, and the retransform does not even withdraw it (wave 27's host run:
// no `exit polls candidate` line, `true` from the wave-26 binary too). The
// one thing such a body DOES copy is an elided constructor: a `new C()` whose
// `C.<init>()V` is exactly `aload_0; invokespecial Object.<init>; return` is
// compiled with no constructor call (`jit_bridge::is_elidable_construction`,
// which also marks `C` copied, so a redefinition of `C` flushes the whole
// cache and withdraws every body). This probe retransforms such a constructor
// into one with a side effect (`made += 1`) while a compiled loop that elided
// it is inside a call, and constructs one right after the call returns, in
// the same iteration.
//
//   parked  a worker's call is `parkOn(idx)` -> `LATCHES[idx].await()`:
//           LATCHES[0] is open, LATCHES[1] (idx == 1 only at iteration WARM,
//           computed without a branch) is released by main after it
//           retransformed MarkerP.
//   self    main's own call is `retransform(inst, args[idx])` ->
//           `inst.retransformClasses(..)`: args[0] is the empty array (the
//           JDK returns at once), args[1] names MarkerS.
// The wrappers carry an exception table so the optimizing tier keeps them
// real calls (a call inside a splice gets no post-call site). The
// instruction after each call is a local load (`int n = idx`), which a
// post-call exit site admits (`post_call_exit_successor_admitted`).
// `ctor-ran-after-the-call` says whether the `new` right after the WARM call
// ran the retransformed constructor.
//
// HotSpot 25 (agent, JIT on and -Xint alike; JDK 25.0.3 locally, 2026-09-27):
//     parked ctor-ran-after-the-call=true
//     self ctor-ran-after-the-call=true
// Without the agent: `no agent`.
//
// CratonVM, expected (not run by the lane that wrote it):
// * `--nojit`: HotSpot's lines.
// * `CRATONVM_JIT_OSR_OPTIMIZING=0` (every OSR body single-pass): HotSpot's
//   lines ONLY because the post-call exit fires. The positive control, under
//   `CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1`:
//     `exit polls candidate: RedefineElidedCtorAfterTheCallProbe.parked... osr=true ir=false withdrawn=true ... post-call-sites=N` (N >= 1)
//     `post-call exit verdict #k: RedefineElidedCtorAfterTheCallProbe.parked... named=true withdrawn=true verdict=0x..` (non-zero)
//   and the same two for `.self`. With `POST_CALL_EXITS_ENABLED = false`
//   (op_invoke.rs) the rows print `false` (the frame leaves only at the back
//   edge after the `new`), which is the negative control.
// * Default (optimizing OSR tier, which elides the constructor too): HotSpot's
//   lines since wave 28, when that tier's OSR-door compiles got post-call
//   exits (stage 3 of
//   docs/known-issues/interpreter/i26-L6-proposal-a-patchable-post-call-exit-for-withdrawn-bodies-20260928.md,
//   `ir_lower.rs::maybe_emit_post_call_exit_site`); the same two dbg lines,
//   with `ir=true` on the candidate line. `false` on a row there means the
//   IR site was refused (`post-call-sites=0` on its candidate line) or never
//   asked (no `post-call exit verdict` line) -- the two causes to tell apart.
// * Before wave 28 (or with both kill switches off, `op_invoke.rs::
//   POST_CALL_EXITS_ENABLED` and `ir_lower.rs::IR_POST_CALL_EXITS_ENABLED`):
//   `false` on each row whose loop ran compiled at the call.
//
// SETUP: a jar whose manifest has
//     Premain-Class: RedefineElidedCtorAfterTheCallProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineElidedCtorAfterTheCallProbe*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar RedefineElidedCtorAfterTheCallProbe
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class RedefineElidedCtorAfterTheCallProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The parked row's class: an empty constructor until retransformed. */
    public static class MarkerP {
        static int made;

        static int count() {
            return made;
        }
    }

    /** The self row's class: an empty constructor until retransformed. */
    public static class MarkerS {
        static int made;

        static int count() {
            return made;
        }
    }

    /**
     * On a RETRANSFORM of MarkerP / MarkerS (never at the initial load),
     * rewrites the empty constructor `aload_0; invokespecial Object.<init>;
     * return` into `aload_0; invokespecial Object.<init>; made += 1; return`.
     */
    static final class Rewrite implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !("RedefineElidedCtorAfterTheCallProbe$MarkerP".equals(className)
                            || "RedefineElidedCtorAfterTheCallProbe$MarkerS".equals(className))) {
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
        // The Fieldref `className.made:I`.
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
        at += 6; // access_flags, this_class, super_class
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
                        return null; // not the empty constructor: leave it
                    }
                    byte hi = (byte) (field >> 8);
                    byte lo = (byte) field;
                    byte[] body = {
                        0x2a, (byte) 0xb7, bytes[code + 2], bytes[code + 3], // aload_0; invokespecial
                        (byte) 0xb2, hi, lo, // getstatic made
                        0x04, 0x60, // iconst_1; iadd
                        (byte) 0xb3, hi, lo, // putstatic made
                        (byte) 0xb1, // return
                    };
                    ByteArrayOutputStream out = new ByteArrayOutputStream();
                    out.write(bytes, 0, a + 2);
                    ByteBuffer attrBytes = ByteBuffer.allocate(4 + 2 + 2 + 4 + body.length + 2 + 2);
                    attrBytes.putInt(2 + 2 + 4 + body.length + 2 + 2);
                    attrBytes.putShort((short) 2); // max_stack
                    attrBytes.putShort((short) maxLocals);
                    attrBytes.putInt(body.length);
                    attrBytes.put(body);
                    attrBytes.putShort((short) 0); // exception_table_length
                    attrBytes.putShort((short) 0); // attributes_count
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

    /** A multiple of 1024, so the call at iteration WARM is made. */
    static final long WARM = 20L << 20;
    static final long AFTER = 2L << 20;
    static volatile boolean parkedIn;
    static final CountDownLatch GO = new CountDownLatch(1);
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static final CountDownLatch[] LATCHES = {OPEN, GO};
    static volatile int[] parkedResult;

    /** 1 at `i == WARM`, else 0, without a branch (0 <= i, WARM < 2^62). */
    static int at(long i) {
        return (int) (((i ^ WARM) - 1) >>> 63);
    }

    /**
     * The parked row's call. A method with an exception table, so that
     * CratonVM's optimizing tier keeps it a REAL call rather than splicing it
     * (its inline resolver refuses such a callee): a call made inside a splice
     * gets no post-call exit site, and `await()` / `retransformClasses` are
     * small enough to be spliced otherwise.
     */
    static void parkOn(int idx) throws InterruptedException {
        try {
            LATCHES[idx].await();
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** The self row's call, kept a real call the same way. */
    static void retransform(Instrumentation inst, Class<?>[] classes) throws Exception {
        try {
            inst.retransformClasses(classes);
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** {made by the last idx-0 `new`, made by the `new` right after the WARM call}. */
    static int[] parked() throws InterruptedException {
        int[] deltas = new int[2];
        for (long i = 0; i < WARM + AFTER; i++) {
            if ((i & 1023) == 0) {
                int idx = at(i);
                parkedIn |= idx == 1;
                int before = MarkerP.made;
                parkOn(idx);
                int n = idx;
                MarkerP m = new MarkerP();
                deltas[n] = MarkerP.made - before;
            }
        }
        return deltas;
    }

    static int[] self(Instrumentation inst) throws Exception {
        Class<?>[][] args = {new Class<?>[0], new Class<?>[] {MarkerS.class}};
        int[] deltas = new int[2];
        for (long k = 0; k < WARM + AFTER; k++) {
            if ((k & 1023) == 0) {
                int idx = at(k);
                int before = MarkerS.made;
                retransform(inst, args[idx]);
                int n = idx;
                MarkerS m = new MarkerS();
                deltas[n] = MarkerS.made - before;
            }
        }
        return deltas;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Rewrite(), true);
        Thread worker = new Thread(() -> {
            try {
                parkedResult = parked();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!parkedIn) {
            Thread.sleep(1);
        }
        // Parked in `GO.await()`, a call made from inside its compiled loop.
        Thread.sleep(50);
        i.retransformClasses(MarkerP.class);
        GO.countDown();
        worker.join();
        int[] p = parkedResult;
        System.out.println("parked ctor-ran-after-the-call=" + (p[1] == 1));
        int[] s = self(i);
        System.out.println("self ctor-ran-after-the-call=" + (s[1] == 1));
    }
}
