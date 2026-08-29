// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;

/**
 * Regression: `VarHandle` access modes must answer identically whether they are
 * served by the generic native funnel or by the JIT's per-call-site fast path
 * (`jit::helpers::try_varhandle_instance_field_read`).
 *
 * A `VarHandle` access is signature-polymorphic: the call site names its own
 * descriptor (`(LHolder;)I`) while the native is registered under an erased
 * `([Ljava/lang/Object;)Ljava/lang/Object;`. Two things now short-circuit that:
 *
 *   1. the per-call-site native cache resolves the erased registration
 *      (`resolve_native_owner_for_receiver`'s rule 4) instead of refusing the
 *      site and re-running `invoke_or_native`'s cascade on every call, and
 *   2. an INSTANCE-field read whose slot is already resolved is served as a
 *      plain field load, skipping the native, its wrapper allocation, and the
 *      unbox that immediately threw the wrapper away.
 *
 * Both are silent when wrong: (2) can hand back a neighbouring slot, a value
 * decoded against the wrong descriptor, or a stale field index, and nothing
 * throws. So every check below asserts an EXACT value chosen to differ if the
 * slot or the descriptor is confused, and every access runs in a loop long
 * enough for the site to warm — a first call served by the funnel and every
 * later one served by the fast path is exactly the shape a one-shot test
 * cannot tell apart.
 *
 * Sections 4-6 are the refusals. Each is a handle shape the fast path must NOT
 * claim (static, array-element, byte-array view), and each would produce a
 * plausible wrong number if it did.
 */
public final class RVarHandleAccess {

    // ---- 1. every primitive width, plus a reference, on one object --------
    static final class Holder {
        boolean z = true;
        byte b = (byte) -7;
        char c = 'Q';
        short s = (short) -300;
        int i = 0x0BADF00D;
        long j = 0x0123456789ABCDEFL;
        float f = 3.5f;
        double d = -2.25d;
        String ref = "holder-ref";
        // A neighbour on each side of `i` whose value is distinguishable, so a
        // one-slot error is visible rather than plausible.
        int before = 111111;
        int after = 222222;
    }

    static final MethodHandles.Lookup L = MethodHandles.lookup();
    static VarHandle vh(String name, Class<?> type) {
        try {
            return L.findVarHandle(Holder.class, name, type);
        } catch (ReflectiveOperationException e) {
            throw new AssertionError(e);
        }
    }

    static final VarHandle VZ = vh("z", boolean.class);
    static final VarHandle VB = vh("b", byte.class);
    static final VarHandle VC = vh("c", char.class);
    static final VarHandle VS = vh("s", short.class);
    static final VarHandle VI = vh("i", int.class);
    static final VarHandle VJ = vh("j", long.class);
    static final VarHandle VF = vh("f", float.class);
    static final VarHandle VD = vh("d", double.class);
    static final VarHandle VR = vh("ref", String.class);
    static final VarHandle VBEFORE = vh("before", int.class);
    static final VarHandle VAFTER = vh("after", int.class);

    // ---- 2. a static-field handle: the fast path must decline -------------
    static int staticInt = 0xFEEDBEEF;
    static final VarHandle VSTATIC;
    // ---- 3. an array-element handle: also declined -------------------------
    static final VarHandle VARRAY = MethodHandles.arrayElementVarHandle(int[].class);
    // ---- 4. a byte-array view handle: also declined ------------------------
    static final VarHandle VVIEW =
            MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);

    static {
        try {
            VSTATIC = L.findStaticVarHandle(RVarHandleAccess.class, "staticInt", int.class);
        } catch (ReflectiveOperationException e) {
            throw new AssertionError(e);
        }
    }

    static int checks;

    /**
     * Assert AND publish. The suite diffs the two VMs through a filter that
     * keeps only `PASS `/`CK ` lines, so an assertion whose value never reaches
     * stdout is invisible to the cross-VM comparison — a vector that printed
     * only its verdict would diff a constant against itself. Every checked
     * value therefore goes out on its own `CK` line.
     */
    static void eq(String what, Object expected, Object actual) {
        checks++;
        System.out.println("CK RVarHandleAccess " + what + "=" + actual);
        if (!expected.equals(actual)) {
            throw new AssertionError(what + ": expected " + expected + " got " + actual);
        }
    }

    /** Enough iterations that the call site is compiled and its cache warm. */
    static final int WARM = 60000;

    public static void main(String[] args) {
        Holder h = new Holder();

        // 1. Reads of every declared width. Each runs WARM times so the first
        //    (funnel-served) answer and the steady-state (fast-path-served)
        //    answer are both covered, and the LAST one is the one asserted.
        boolean z = false;
        byte b = 0;
        char c = 0;
        short s = 0;
        int i = 0;
        long j = 0;
        float f = 0;
        double d = 0;
        Object r = null;
        for (int n = 0; n < WARM; n++) {
            z = (boolean) VZ.get(h);
            b = (byte) VB.get(h);
            c = (char) VC.get(h);
            s = (short) VS.get(h);
            i = (int) VI.get(h);
            j = (long) VJ.get(h);
            f = (float) VF.get(h);
            d = (double) VD.get(h);
            r = VR.get(h);
        }
        eq("get boolean", Boolean.TRUE, z);
        eq("get byte", (byte) -7, b);
        eq("get char", 'Q', c);
        eq("get short", (short) -300, s);
        eq("get int", 0x0BADF00D, i);
        eq("get long", 0x0123456789ABCDEFL, j);
        eq("get float", 3.5f, f);
        eq("get double", -2.25d, d);
        eq("get ref", "holder-ref", r);

        // 1b. The three adjacent int slots, read through three different
        //     handles at three different call sites. A shared or mis-keyed
        //     plan shows up here and nowhere else.
        int before = 0, mid = 0, after = 0;
        for (int n = 0; n < WARM; n++) {
            before = (int) VBEFORE.get(h);
            mid = (int) VI.get(h);
            after = (int) VAFTER.get(h);
        }
        eq("neighbour before", 111111, before);
        eq("neighbour mid", 0x0BADF00D, mid);
        eq("neighbour after", 222222, after);

        // 1c. The other read modes share one native and one fast path; a
        //     mode-blind plan would still answer, so assert each separately.
        int acq = 0, opq = 0, vol = 0;
        for (int n = 0; n < WARM; n++) {
            acq = (int) VI.getAcquire(h);
            opq = (int) VI.getOpaque(h);
            vol = (int) VI.getVolatile(h);
        }
        eq("getAcquire", 0x0BADF00D, acq);
        eq("getOpaque", 0x0BADF00D, opq);
        eq("getVolatile", 0x0BADF00D, vol);

        // 2. Writes still go through the native. Interleaving them with reads
        //    at a warm site is what catches a read fast path that answers from
        //    anything staler than the heap.
        int roundTripped = -1;
        for (int n = 0; n < WARM; n++) {
            VI.set(h, n);
            int back = (int) VI.get(h);
            if (back != n) {
                throw new AssertionError("set/get round trip at n=" + n + " read " + back);
            }
            roundTripped = back;
        }
        eq("set/get round trip last", WARM - 1, roundTripped);
        VI.set(h, 0x0BADF00D);

        // 3. compareAndSet / getAndAdd — the mutating modes, unchanged, but
        //    they share the site-cache entry shape with the reads above.
        boolean cas = VI.compareAndSet(h, 0x0BADF00D, 42);
        eq("compareAndSet hit", Boolean.TRUE, cas);
        eq("compareAndSet result", 42, (int) VI.get(h));
        eq("compareAndSet miss", Boolean.FALSE, VI.compareAndSet(h, 999, 7));
        eq("getAndAdd", 42, (int) VI.getAndAdd(h, 5));
        eq("getAndAdd result", 47, (int) VI.get(h));

        // 3b. compareAndSet on a REFERENCE field.
        //
        // Separate from the `int` CAS above because the payload path is
        // different, not because the API is: a reference CAS fires the SATB
        // pre-barrier on `expected` BEFORE the store and the post
        // `write_barrier` only on success, and it is the shape
        // `CompletableFuture.tryPushStack` runs on every push.
        //
        // Warmed in a loop so the compiled route is the one under test — a
        // handful of calls would measure the interpreter and say nothing about
        // the bind. The alternation is deliberate: a CAS that always writes the
        // value already there would pass against an implementation that never
        // stores at all.
        // Save and restore: a later section asserts what `ref` holds, so this
        // block must leave the field exactly as it found it.
        String savedRef = (String) VR.get(h);
        String sa = "alpha";
        String sb = "beta";
        VR.set(h, sa);
        int hits = 0;
        int misses = 0;
        for (int n = 0; n < WARM; n++) {
            String cur = (n & 1) == 0 ? sa : sb;
            String nxt = (n & 1) == 0 ? sb : sa;
            if (VR.compareAndSet(h, cur, nxt)) {
                hits++;
            }
            // A stale expected value must NOT swap, and must not disturb the
            // field either — the miss path still runs the pre-barrier.
            if (VR.compareAndSet(h, "never-stored", sa)) {
                misses++;
            }
        }
        eq("ref CAS hits", WARM, hits);
        eq("ref CAS misses swapped", 0, misses);
        eq("ref CAS final", sa, (String) VR.get(h));
        // null is a legal reference operand on both sides of the compare.
        eq("ref CAS to null", Boolean.TRUE, VR.compareAndSet(h, sa, null));
        // `eq` compares with `expected.equals(actual)`, so a null EXPECTED
        // would NPE inside the harness rather than assert anything. Publish the
        // nullness as a boolean instead.
        eq("ref CAS null read", Boolean.TRUE, VR.get(h) == null);
        eq("ref CAS from null", Boolean.TRUE, VR.compareAndSet(h, null, sb));
        eq("ref CAS from null read", sb, (String) VR.get(h));
        VR.set(h, savedRef);

        // 4. A static-field handle has no receiver coordinate. If the fast
        //    path ever claimed one it would read slot N of the *handle*.
        int st = 0;
        for (int n = 0; n < WARM; n++) {
            st = (int) VSTATIC.get();
        }
        eq("static get", 0xFEEDBEEF, st);

        // 5. Array-element access: the coordinate is an index, not a field slot.
        int[] arr = {5, 6, 7, 8};
        int a1 = 0, a3 = 0;
        for (int n = 0; n < WARM; n++) {
            a1 = (int) VARRAY.get(arr, 1);
            a3 = (int) VARRAY.get(arr, 3);
        }
        eq("array elem 1", 6, a1);
        eq("array elem 3", 8, a3);

        // 6. A byte-array view reads `width` bytes at a BYTE index — the one
        //    shape whose arguments look exactly like a field read plus an int.
        byte[] raw = new byte[16];
        raw[4] = 0x11;
        raw[5] = 0x22;
        raw[6] = 0x33;
        raw[7] = 0x44;
        int viewed = 0;
        for (int n = 0; n < WARM; n++) {
            viewed = (int) VVIEW.get(raw, 4);
        }
        eq("byte view LE @4", 0x44332211, viewed);

        // 7. A reference read whose site descriptor is `Object` — the boxing
        //    shape the fast path must not shortcut into a primitive slot.
        Object asObject = null;
        for (int n = 0; n < WARM; n++) {
            asObject = VR.get(h);
        }
        eq("ref as Object", "holder-ref", asObject);

        // 8. Two receivers of the SAME class through one handle: the plan is a
        //    property of the handle, never of the receiver it was first seen
        //    with.
        Holder other = new Holder();
        other.i = 0x1234;
        int mine = 0, theirs = 0;
        for (int n = 0; n < WARM; n++) {
            mine = (int) VI.get(h);
            theirs = (int) VI.get(other);
        }
        eq("receiver A", 47, mine);
        eq("receiver B", 0x1234, theirs);

        System.out.println("CK RVarHandleAccess checks=" + checks);
        System.out.println("PASS RVarHandleAccess");
    }
}
