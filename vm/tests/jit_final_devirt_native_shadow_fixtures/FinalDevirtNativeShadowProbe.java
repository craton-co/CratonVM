// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.InetSocketAddress;
import java.nio.channels.DatagramChannel;
import java.nio.channels.SelectableChannel;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.channels.spi.SelectorProvider;

/**
 * The interpreter and compiled code must answer the same channel question the
 * same way.
 *
 * Both loops below are netty's own shape: a small delegating method, called
 * often enough that the JIT compiles it. `isOpen()` and `close()` are both
 * `public final` on `java.nio.channels.spi.AbstractInterruptibleChannel`, so
 * the final-method devirtualiser can bind the site straight to the JDK
 * classfile body -- past the native CratonVM registers on the channel classes,
 * which is what the interpreter dispatches to.
 *
 * Before the 2026-09-05 fix the two loops read, on a release binary:
 *   isOpen  : 397,739 OPEN answers in 400,000 calls on a CLOSED channel,
 *             first at call 2,261 (the JDK body reads a `closed` field the
 *             native close path never wrote).
 *   close   : 3,487 NullPointerExceptions in 4,000 closes, first at call 512
 *             (the JDK body opens with `synchronized (closeLock)`, a field the
 *             datagram factory never seeded), each leaving the socket open.
 *
 * Netty turned the pair into
 * `IllegalStateException: close() must be invoked after the channel is closed.`
 * in three unrelated test classes. HotSpot answers 0 for every row here.
 */
public class FinalDevirtNativeShadowProbe {

    /** `AbstractNioChannel.isOpen()` is exactly this. */
    static boolean askSelectable(SelectableChannel c) {
        return c.isOpen();
    }

    /** `NioDatagramChannel.doClose()` is exactly this. */
    static void doCloseLikeNetty(SelectableChannel c) throws Exception {
        c.close();
    }

    /** How many of `reps` calls disagreed with the interpreter's answer. */
    static int openAnswersAfterClose(SelectableChannel closed, int reps) {
        int wrong = 0;
        for (int i = 0; i < reps; i++) {
            if (askSelectable(closed)) {
                wrong++;
            }
        }
        return wrong;
    }

    static int failures;

    static void check(String what, int wrong) {
        if (wrong != 0) {
            failures++;
            System.out.println("FAIL " + what + " wrong=" + wrong);
        } else {
            System.out.println("ok   " + what);
        }
    }

    public static void main(String[] args) throws Exception {
        int reps = Integer.getInteger("reps", 40000);
        int closes = Integer.getInteger("closes", 3000);

        SocketChannel sc = SocketChannel.open();
        sc.close();
        check("SocketChannel.isOpen after close", openAnswersAfterClose(sc, reps));

        ServerSocketChannel ssc = ServerSocketChannel.open();
        ssc.bind(new InetSocketAddress("127.0.0.1", 0));
        ssc.close();
        check("ServerSocketChannel.isOpen after close", openAnswersAfterClose(ssc, reps));

        DatagramChannel dc = DatagramChannel.open();
        dc.close();
        check("DatagramChannel.isOpen after close", openAnswersAfterClose(dc, reps));

        // A live channel must keep reading OPEN just as hard: a screen that
        // simply answered "closed" everywhere would pass the three rows above.
        SocketChannel live = SocketChannel.open();
        int liveWrong = 0;
        for (int i = 0; i < reps; i++) {
            if (!askSelectable(live)) {
                liveWrong++;
            }
        }
        check("SocketChannel.isOpen while open", liveWrong);
        live.close();

        // The close half. Each iteration opens a fresh datagram channel and
        // closes it through the delegating method; a throw, or a channel still
        // reading open afterwards, is the defect.
        SelectorProvider provider = SelectorProvider.provider();
        int threw = 0;
        int stillOpen = 0;
        for (int i = 0; i < closes; i++) {
            DatagramChannel d;
            try {
                d = provider.openDatagramChannel();
            } catch (Throwable openFailed) {
                // Running out of sockets IS the defect's own footprint (a
                // close that throws leaks the fd), so say so rather than
                // reporting a clean run over a truncated loop.
                failures++;
                System.out.println("FAIL openDatagramChannel at i=" + i + ": " + openFailed);
                break;
            }
            try {
                doCloseLikeNetty(d);
            } catch (Throwable t) {
                threw++;
            }
            if (d.isOpen()) {
                stillOpen++;
            }
        }
        check("DatagramChannel.close threw", threw);
        check("DatagramChannel still open after close", stillOpen);

        if (failures == 0) {
            System.out.println("FINAL_DEVIRT_NATIVE_SHADOW_OK");
        }
    }
}
