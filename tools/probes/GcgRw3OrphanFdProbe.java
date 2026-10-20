// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.channels.DatagramChannel;

/**
 * gcg g3/rw (2026-10-01): the gate of
 * {@code common-w16c-proposal-post-gc-native-cleanup-hook} -- "a dead
 * {@code DatagramChannel}'s socket closed within one collection without
 * another DatagramChannel call".
 *
 * <p>Opens a loopback {@code DatagramChannel}, drops it without
 * {@code close()}, collects, churns a little (so an allocation-triggered
 * collection runs too) and then HOLDS for {@code holdMs} without touching any
 * channel. HotSpot's {@code Cleaner} closes the socket shortly after the
 * collection that found the channel dead; CratonVM parks the fd
 * ({@code native-io} {@code io_orphan_fds}) and, without
 * {@code CRATONVM_GC_POST_GC_CLOSE_ORPHAN_FDS=1}, closes it only at the next
 * DatagramChannel / synthetic-RAF native, i.e. never in this program.
 *
 * <p>stdout is deterministic ({@code orphan-fd PROBE-OK}). The decision is
 * the OS's: the port goes to stderr ({@code orphan-fd port=N}), followed by
 * {@code orphan-fd hold} when the hold begins; during the hold the harness
 * asks {@code ss -Hun "sport = :N"} whether the socket is still bound (see
 * the {@code rw3_orphanfd_*} rows in
 * {@code docs/internal/gc-round-gcg-20261001/g3-rw-report.md}).
 *
 * <p>Usage: {@code GcgRw3OrphanFdProbe [holdMs] [keep]} (default 4000). With
 * {@code keep} the channel stays strongly reachable: the measurement's
 * positive control (the port must still be bound during the hold, on every
 * VM).
 */
public class GcgRw3OrphanFdProbe {
    static DatagramChannel kept;

    public static void main(String[] args) throws Exception {
        int holdMs = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        boolean keep = args.length > 1 && args[1].equals("keep");
        int port = openAndDrop(keep);
        System.err.println("orphan-fd port=" + port);
        for (int i = 0; i < 3; i++) {
            System.gc();
            Thread.sleep(50);
        }
        long sink = 0;
        for (int i = 0; i < 400_000; i++) {
            sink += new byte[64].length;
        }
        System.err.println("orphan-fd hold sink=" + sink);
        Thread.sleep(holdMs);
        System.out.println("orphan-fd PROBE-OK");
    }

    /**
     * Bound, then unreachable on return (unless {@code keep}): never closed by
     * the program.
     */
    private static int openAndDrop(boolean keep) throws Exception {
        DatagramChannel ch = DatagramChannel.open();
        ch.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        if (keep) {
            kept = ch;
        }
        return ((InetSocketAddress) ch.getLocalAddress()).getPort();
    }
}
