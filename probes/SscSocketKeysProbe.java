import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;

/**
 * The read side of W7-72-ssc-socket-and-filechannel.md item 1: does
 * {@code ServerSocketChannel.socket()} damage {@code keys}?
 *
 * <p><b>Why every read here goes through real JDK bytecode.</b>
 * CratonVM's {@code ssc_socket} used to cache the {@code java.net.ServerSocket}
 * adaptor in slot 5 of the channel object, which on JDK 25.0.3.9 is
 * {@code AbstractSelectableChannel.keys} — a {@code SelectionKey[]}. Reading
 * that slot back through the same native that wrote it proves nothing: the pair
 * agrees no matter how wrong the index is. That is the vacuous shape
 * {@code SlotIndexRecensusProbe.java} was written to avoid, and this probe
 * follows the same rule.
 *
 * <p>{@code register}, {@code isRegistered}, {@code keyFor} and
 * {@code implCloseChannel} are all {@code AbstractSelectableChannel} bytecode
 * that indexes {@code keys} by the JDK's own field index. CratonVM registers no
 * native on any of them, so they observe precisely the slot the native wrote,
 * through the JDK's index. {@code isRegistered()} additionally reads
 * {@code keyCount}, which is slot 6 — one past the clobbered one — so the two
 * together separate "the array is wrong" from "the count is wrong".
 *
 * <p><b>Both orders are exercised, because the defect is order-sensitive and a
 * one-order probe passes for the wrong reason.</b> With the clobber in place:
 * socket()-then-register() hands {@code register} a {@code ServerSocket} where
 * it expects a {@code SelectionKey[]}; register()-then-socket() lets the
 * registration succeed and then overwrites the live key array, so it is the
 * LATER {@code keyFor}/{@code close} that fails. A probe that only did one of
 * these would report green on half a corrupt VM.
 *
 * <p>Section 3 is the cache-identity guard: {@code socket()} must keep
 * answering the SAME {@code ServerSocket} on every call
 * ({@code ServerSocketChannel.socket()}'s contract). Moving the cache off the
 * object and into a side table is the repair, and a repair that lost the cache
 * would be a different regression — so the probe asserts identity, not merely
 * non-null.
 *
 * <p>Run on HotSpot 25.0.3+9 to establish the oracle; the transcript is at the
 * bottom of this file. Every line prints a VALUE, so the HotSpot and CratonVM
 * transcripts diff directly.
 */
public class SscSocketKeysProbe {

    static String show(ThrowingSupplier<?> s) {
        try {
            return String.valueOf(s.get());
        } catch (Throwable t) {
            return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
    }

    interface ThrowingSupplier<T> {
        T get() throws Throwable;
    }

    public static void main(String[] args) throws Exception {
        socketThenRegister();
        registerThenSocket();
        cacheIdentity();
        closeWalksKeys();
    }

    // -- 1 -- socket() first, then register(): does the registration survive a
    //         write that landed in `keys`?
    static void socketThenRegister() {
        System.out.println("== 1 socket() then register() ==");
        try (Selector sel = Selector.open();
             ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            ssc.configureBlocking(false);

            ServerSocket ss = ssc.socket();
            System.out.println("  socket.class            = " + ss.getClass().getName());
            System.out.println("  socket.isBound          = " + show(ss::isBound));

            System.out.println("  isRegistered.before     = " + show(ssc::isRegistered));
            SelectionKey k = (SelectionKey) tryRegister(ssc, sel);
            System.out.println("  register                = " + (k == null ? "<threw>" : "ok"));
            System.out.println("  isRegistered.after      = " + show(ssc::isRegistered));
            System.out.println("  keyFor.sameKey          = "
                    + show(() -> k != null && ssc.keyFor(sel) == k));
            System.out.println("  keyFor.nonNull          = " + show(() -> ssc.keyFor(sel) != null));
            // The channel must still answer the same ServerSocket afterwards:
            // repairing `keys` by dropping the cache would be its own defect.
            System.out.println("  socket.stable           = " + show(() -> ssc.socket() == ss));
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        }
    }

    // -- 2 -- register() first, then socket(): the write lands on a LIVE key
    //         array, so the damage shows up on the reads that follow.
    static void registerThenSocket() {
        System.out.println("== 2 register() then socket() ==");
        try (Selector sel = Selector.open();
             ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            ssc.configureBlocking(false);

            SelectionKey k = (SelectionKey) tryRegister(ssc, sel);
            System.out.println("  register                = " + (k == null ? "<threw>" : "ok"));
            System.out.println("  keyFor.beforeSocket     = " + show(() -> ssc.keyFor(sel) == k));

            ServerSocket ss = ssc.socket();
            System.out.println("  socket.class            = " + ss.getClass().getName());
            // THE discriminating read: `keyFor` walks `keys` by the JDK's index,
            // after socket() has had its chance to overwrite it.
            System.out.println("  keyFor.afterSocket      = " + show(() -> ssc.keyFor(sel) == k));
            System.out.println("  isRegistered.afterSocket= " + show(ssc::isRegistered));
            System.out.println("  key.isValid             = " + show(() -> k != null && k.isValid()));
            System.out.println("  key.channel.isSame      = " + show(() -> k != null && k.channel() == ssc));
            System.out.println("  socket.stable           = " + show(() -> ssc.socket() == ss));
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        }
    }

    // -- 3 -- socket() is specified to answer the same object every time.
    static void cacheIdentity() {
        System.out.println("== 3 socket() cache identity ==");
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            ServerSocket a = ssc.socket();
            ServerSocket b = ssc.socket();
            ServerSocket c = ssc.socket();
            System.out.println("  a==b                    = " + (a == b));
            System.out.println("  b==c                    = " + (b == c));
            System.out.println("  localPort.matches       = "
                    + show(() -> a.getLocalPort() == ((InetSocketAddress) ssc.getLocalAddress()).getPort()));
            // A second channel must get its OWN adaptor: an identity-hash-keyed
            // side table whose rows were not disambiguated by the receiver would
            // hand this one the first channel's socket.
            try (ServerSocketChannel other = ServerSocketChannel.open()) {
                other.bind(new InetSocketAddress("127.0.0.1", 0));
                System.out.println("  distinctChannels        = " + (other.socket() != a));
            }
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        }
    }

    // -- 4 -- implCloseChannel() walks `keys` to cancel them. A ServerSocket
    //         sitting there is not a SelectionKey[] and close() is where a
    //         server actually notices.
    static void closeWalksKeys() {
        System.out.println("== 4 close() after socket()+register() ==");
        Selector sel = null;
        try {
            sel = Selector.open();
            ServerSocketChannel ssc = ServerSocketChannel.open();
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            ssc.configureBlocking(false);
            ssc.socket();
            SelectionKey k = (SelectionKey) tryRegister(ssc, sel);
            System.out.println("  registered              = " + show(ssc::isRegistered));
            ssc.close();
            System.out.println("  closed.isOpen           = " + show(ssc::isOpen));
            System.out.println("  key.validAfterClose     = " + show(() -> k != null && k.isValid()));
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        } finally {
            if (sel != null) {
                try {
                    sel.close();
                } catch (Throwable ignored) {
                    // closing the selector is teardown, not an observation
                }
            }
        }
    }

    static Object tryRegister(ServerSocketChannel ssc, Selector sel) {
        try {
            return ssc.register(sel, SelectionKey.OP_ACCEPT);
        } catch (Throwable t) {
            System.out.println("  register threw          = " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
            return null;
        }
    }
}

/*
HotSpot oracle — Temurin 25.0.3+9 (Windows 11), measured 2026-08-12.
`javap -version` = 25.0.3. Command: java probes/SscSocketKeysProbe.java

== 1 socket() then register() ==
  socket.class            = sun.nio.ch.ServerSocketAdaptor
  socket.isBound          = true
  isRegistered.before     = false
  register                = ok
  isRegistered.after      = true
  keyFor.sameKey          = true
  keyFor.nonNull          = true
  socket.stable           = true
== 2 register() then socket() ==
  register                = ok
  keyFor.beforeSocket     = true
  socket.class            = sun.nio.ch.ServerSocketAdaptor
  keyFor.afterSocket      = true
  isRegistered.afterSocket= true
  key.isValid             = true
  key.channel.isSame      = true
  socket.stable           = true
== 3 socket() cache identity ==
  a==b                    = true
  b==c                    = true
  localPort.matches       = true
  distinctChannels        = true
== 4 close() after socket()+register() ==
  registered              = true
  closed.isOpen           = false
  key.validAfterClose     = false

Reading it. Every `true` above except the two `socket.class` lines and
`closed.isOpen`/`key.validAfterClose` is a RED on a VM with the slot-5 clobber:
`keyFor` and `isRegistered` are `AbstractSelectableChannel` bytecode over
`keys`/`keyCount`, and a `ServerSocket` in `keys` cannot answer them.

`socket.class` is INFORMATIONAL, not a red. CratonVM answers
`sun.nio.ch.ServerSocketAdaptor` only under `CRATONVM_REAL_NET_SOCKETS`; the
default real-JDK arm hands back a bare `java.net.ServerSocket`. That difference
predates this repair and is not what the probe is for.

`closed.isOpen = false` and `key.validAfterClose = false` are the two lines
whose HotSpot value is `false`; a VM answering `true` there has a `close()` that
did not walk `keys`.
*/
