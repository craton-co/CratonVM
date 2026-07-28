import java.lang.reflect.Field;
import java.net.InetSocketAddress;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

/**
 * Regression: BUG-NIO-NULL-INTERRUPTOR-20260726, socket half.
 *
 * `RChannelInterrupt` closed the `FileChannelImpl` half of this on 2026-07-26.
 * `SocketChannel` / `ServerSocketChannel` are built by the same bridge and were
 * left with the same null `AbstractInterruptibleChannel.interruptor`, which
 * `begin()` dereferences unconditionally once the calling thread's interrupt
 * flag is set. They were deliberately left open at the time: seeding the field
 * needs an allocation, and the shared `init_channel_locks` carries an
 * empirically-earned warning against allocating on that path (an allocation
 * there once relocated the channel under concurrent load and left `closeLock`
 * null, killing the Apache httpasyncclient reactor).
 *
 * Fixed 2026-07-27 by sequencing the allocation AFTER the three monitor fields
 * and pinning the channel across it.
 *
 * The assertion is on the field itself, because CratonVM services these channel
 * operations natively — `begin()` may never run, so a behavioural probe cannot
 * tell a seeded channel from an unseeded one. Reading a `java.base` internal
 * needs `--add-opens java.base/java.nio.channels.spi=ALL-UNNAMED`, which the
 * suite does not pass, so a blocked read is reported as OK: HotSpot is the
 * reference here, not the subject. HotSpot WITH the flags was checked by hand
 * and reports `AbstractInterruptibleChannel$1` on all three channels, which is
 * what the seeded assertion below demands of CratonVM.
 */
public class RSocketChannelInterrupt {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RSocketChannelInterrupt: " + m);
        }
    }

    /** Sentinel: the field exists but this VM will not let us read it. */
    static final Object BLOCKED = new Object();

    /** Read a field declared anywhere in the channel's ancestry. */
    static Object peek(Object o, String name) {
        for (Class<?> c = o.getClass(); c != null; c = c.getSuperclass()) {
            Field f;
            try {
                f = c.getDeclaredField(name);
            } catch (NoSuchFieldException ignored) {
                continue; // keep walking up
            }
            try {
                f.setAccessible(true);
                return f.get(o);
            } catch (RuntimeException | IllegalAccessException blocked) {
                return BLOCKED;
            }
        }
        return null;
    }

    static void assertSeeded(Object ch, String what) {
        Object interruptor = peek(ch, "interruptor");
        check(interruptor != null,
                what + ".interruptor is null — AbstractInterruptibleChannel.begin() "
                        + "NPEs on any thread whose interrupt flag is set");
        // Not counted: it only runs where reflection is permitted, and the
        // check tally is part of the cross-VM output diff.
        if (interruptor != BLOCKED) {
            String owner = interruptor.getClass().getName();
            if (!owner.startsWith("java.nio.channels.spi.AbstractInterruptibleChannel$")) {
                throw new AssertionError("RSocketChannelInterrupt: " + what
                        + ".interruptor is not the JDK's own Interruptible: " + owner);
            }
        }
        check(peek(ch, "closeLock") != null, what + ".closeLock is null");
        System.out.println("CK " + what + " interruptor-seeded closeLock-seeded");
    }

    public static void main(String[] args) throws Exception {
        try (SocketChannel sc = SocketChannel.open()) {
            check(sc.isOpen(), "SocketChannel.open() must return an open channel");
            assertSeeded(sc, "SocketChannel");
        }

        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            check(ssc.isOpen(), "ServerSocketChannel.open() must return an open channel");
            assertSeeded(ssc, "ServerSocketChannel");

            // bind + accept builds a THIRD channel through the same bridge path.
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            ssc.configureBlocking(false);
            int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();
            check(port > 0, "bound ServerSocketChannel must report a local port");

            try (SocketChannel client = SocketChannel.open()) {
                client.configureBlocking(true);
                client.connect(new InetSocketAddress("127.0.0.1", port));
                SocketChannel accepted = null;
                for (int i = 0; i < 400 && accepted == null; i++) {
                    accepted = ssc.accept();
                    if (accepted == null) {
                        Thread.sleep(5);
                    }
                }
                check(accepted != null, "accept() produced no channel");
                assertSeeded(accepted, "accepted-SocketChannel");
                accepted.close();
            }
        }

        System.out.println("CK RSocketChannelInterrupt checks=" + checks);
        System.out.println("PASS RSocketChannelInterrupt");
    }
}
