// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;

/**
 * gch h3/fd (2026-10-08,
 * {@code gcf-f1f-debugger-local-reads-export-liveness-dead-stale-locals-20261001}):
 * a reference local that is IN SCOPE (the LocalVariableTable names it, so a
 * debugger lists it) but DEAD by bytecode liveness (never read again), across
 * two {@code System.gc()} calls, read by a debugger at a breakpoint after them.
 * HotSpot keeps every local alive while an agent can access locals
 * ({@code can_access_local_variables}): jdb's {@code locals} shows the array and
 * {@code print dead.length} answers {@code 1048576}. Before gch h1/jp a
 * CratonVM root scan dropped the dead local, so the array could be collected
 * and the debugger was shown {@code null} or whatever was allocated over it.
 *
 * <p>Compile with {@code javac -g}. The breakpoint line is the one marked
 * {@code BREAK} below (line 45); keep it there or update the MANUAL block in
 * {@code docs/internal/gc-round-gch-20261008/h3-fd-report.md}.
 *
 * <p>With {@code -Dgo=<path>} the probe waits (50 ms polls, at most 60 s) for
 * that file to exist before it runs, so a debugger can attach and set the
 * breakpoint first (CratonVM's {@code --jdwp-suspend} is not implemented).
 * Without it, it runs at once: the plain battery row.
 *
 * <p>STDOUT (deterministic, identical on HotSpot 25):
 * <pre>
 *   marker=1048576
 *   done
 * </pre>
 */
public class GchH3fdDeadLocalJdwpProbe {
    static int sink;

    static void probe() {
        byte[] dead = new byte[1 << 20];
        dead[0] = 7;
        sink = dead.length; // the last read of `dead`
        for (int i = 0; i < 64; i++) {
            churn(i);
        }
        System.gc();
        System.gc();
        int marker = sink; // BREAK (line 45): `dead` in scope, liveness-dead
        System.out.println("marker=" + marker);
    }

    /** Young garbage between the last read and the collections. */
    static void churn(int i) {
        Object[] junk = new Object[256];
        for (int k = 0; k < junk.length; k++) {
            junk[k] = new byte[64 + (k & 7)];
        }
        if (junk[i & 255] == null) {
            sink++;
        }
    }

    public static void main(String[] args) throws Exception {
        String go = System.getProperty("go");
        if (go != null) {
            File f = new File(go);
            for (int i = 0; i < 1200 && !f.exists(); i++) {
                Thread.sleep(50);
            }
        }
        probe();
        System.out.println("done");
    }
}
