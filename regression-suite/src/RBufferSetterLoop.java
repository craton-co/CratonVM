// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// A hot loop over CharBuffer.position / limit must not end interpreted for the
// rest of the process. Before the fix an abandoned C2 supersede spent one of
// the method's three tier_fail_count retries; a later callee deopt (Buffer.position
// speculation) withdrew the baseline body with its raw callers, the method was
// then compile-failed for good and each call cost 2.5-5 us instead of ~30 ns.
// Output is deterministic (a checksum); the timing check is a generous bound.

import java.nio.CharBuffer;

public class RBufferSetterLoop {
    static int sink;

    static long run(CharBuffer c, int n) {
        long t = System.nanoTime();
        for (int i = 0; i < n; i++) {
            c.position(i & 7);
            c.limit(8 + (i & 7));
            sink += c.remaining();
        }
        return System.nanoTime() - t;
    }

    public static void main(String[] a) {
        CharBuffer c = CharBuffer.allocate(16);
        int n = 300_000;
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 6; r++) {
            sink = 0;
            best = Math.min(best, run(c, n));
        }
        // 10x headroom over the fixed ~30-100 ns/op; the defect was 2500+ ns/op.
        System.out.println("CK checksum=" + sink);
        System.out.println(best / n < 1000 ? "PASS RBufferSetterLoop (2 checks)" : "SLOW " + best / n + " ns/op");
    }
}
