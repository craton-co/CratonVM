// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Correctness probe for the jit_buffer_session_direct fast path
// (vm/src/jit/helpers.rs, java.nio.Buffer.session()) landed 2026-09-21 for
// docs/internal/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md
// (retired), "What landed" section 3 (formerly "What is left" item 2).
//
// heapSum/directSum/segSum/viaLayout must match HotSpot exactly in every arm
// (--jdk-only, --nojit, default) -- that is what proves the null-segment fast path
// answers correctly and a non-null segment still reads/writes through real bytecode.
//
// closedThrows is a KNOWN, PRE-EXISTING gap, not a property of this fast path: measured
// 2026-09-21, CratonVM does not raise IllegalStateException on a closed-arena access even
// under --jdk-only --nojit (interpreter only, this helper never runs) -- so it reproduces
// identically with and without the jit_buffer_session_direct change. This is
// docs/known-issues/jdk-only/W7-89-memorysession-checkvalidstate.md §6.3's "ByteBuffer
// view of a closed segment" residual, tracked and scoped there; do not attribute a
// MISSING_EXCEPTION_AFTER_CLOSE line here to this fast path without re-checking against
// --nojit first, the way this probe's own history did.
//
//   javac -d out BufferSessionScopeProbe.java
//   java -cp out BufferSessionScopeProbe   (HotSpot: the oracle)
//   cratonvm [--jdk-only] [--nojit] -cp out BufferSessionScopeProbe
//
// heapSum/directSum/segSum/viaLayout must be identical in all of those; closedThrows
// currently is not (see above).
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class BufferSessionScopeProbe {
    public static void main(String[] args) {
        StringBuilder out = new StringBuilder();

        // 1) Heap buffer: segment == null on every access. Exercise many round trips so a
        //    JIT-compiled accessor is exercised, not just the interpreter.
        ByteBuffer heap = ByteBuffer.allocate(64).order(ByteOrder.LITTLE_ENDIAN);
        long heapSum = 0;
        for (int i = 0; i < 200_000; i++) {
            int v = i * 2654435761L > 0 ? i : -i;
            heap.putInt(0, v);
            heapSum += heap.getInt(0);
        }
        out.append("heapSum=").append(heapSum).append('\n');

        // 2) Direct buffer, no MemorySegment: also segment == null.
        ByteBuffer direct = ByteBuffer.allocateDirect(64).order(ByteOrder.LITTLE_ENDIAN);
        long directSum = 0;
        for (int i = 0; i < 200_000; i++) {
            direct.putInt(0, i);
            directSum += direct.getInt(0);
        }
        out.append("directSum=").append(directSum).append('\n');

        // 3) A REAL MemorySegment-backed buffer: segment != null. Round-trip while the
        //    arena is open -- must decline to the real bytecode and get the right values.
        // Not try-with-resources: step 4 closes the arena explicitly and a
        // double-close would itself throw before we get to assert the property we want.
        Arena arena = Arena.ofConfined();
        MemorySegment seg = arena.allocate(64, 8);
        ByteBuffer segBuf = seg.asByteBuffer().order(ByteOrder.LITTLE_ENDIAN);
        long segSum = 0;
        for (int i = 0; i < 200_000; i++) {
            segBuf.putInt(0, i * 3);
            segSum += segBuf.getInt(0);
        }
        out.append("segSum=").append(segSum).append('\n');

        // Also via the VarHandle/ValueLayout path some accessors route through, to
        // exercise a second caller of the same session() check.
        long viaLayout = 0;
        for (int i = 0; i < 1_000; i++) {
            seg.set(ValueLayout.JAVA_INT, 0, i);
            viaLayout += seg.get(ValueLayout.JAVA_INT, 0);
        }
        out.append("viaLayout=").append(viaLayout).append('\n');

        // 4) THE critical case: close the arena, then try an accessor on a buffer
        //    still holding that now-invalid segment. Must throw IllegalStateException
        //    -- this is the exact scope-validation property a constant-null shim
        //    would silently skip.
        arena.close();
        try {
            segBuf.getInt(0);
            out.append("MISSING_EXCEPTION_AFTER_CLOSE\n");
        } catch (IllegalStateException e) {
            out.append("closedThrows=IllegalStateException\n");
        }

        System.out.print(out);
    }
}
