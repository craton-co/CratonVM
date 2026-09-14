import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.DatagramChannel;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * A blocking-mode {@code DatagramChannel.receive()} must not lock other
 * channels out of the VM's UDP registry.
 *
 * Park one channel in {@code receive()} with nothing ever sent to it, then
 * time an unrelated open/bind/close on a second channel. If the receive is
 * holding a global registry lock across the syscall, the second channel can
 * never be created and this probe hangs instead of printing DONE.
 */
public class UdpWedgeProbe {
    public static void main(String[] args) throws Exception {
        DatagramChannel parked = DatagramChannel.open();
        parked.bind(new InetSocketAddress("127.0.0.1", 0));
        System.out.println("OK   parked channel bound to " + parked.getLocalAddress()
                + " blocking=" + parked.isBlocking());

        CountDownLatch entered = new CountDownLatch(1);
        Thread receiver = new Thread(() -> {
            try {
                ByteBuffer buf = ByteBuffer.allocate(64);
                entered.countDown();
                parked.receive(buf);      // nothing is ever sent here
            } catch (Throwable t) {
                System.out.println("     (receiver ended: " + t + ")");
            }
        }, "parked-receiver");
        receiver.setDaemon(true);
        receiver.start();
        entered.await(5, TimeUnit.SECONDS);
        Thread.sleep(500);                // let it actually enter the syscall

        long t0 = System.nanoTime();
        DatagramChannel other = DatagramChannel.open();
        other.bind(new InetSocketAddress("127.0.0.1", 0));
        System.out.println("OK   second channel opened while the first is parked, local="
                + other.getLocalAddress());
        other.close();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println((ms < 1000 ? "OK   " : "FAIL ") + "open+bind+close took " + ms + "ms");

        // A send to the parked channel must still be delivered. Address it by
        // port on explicit loopback: CratonVM reports the parked channel's
        // getLocalAddress() as the wildcard 0.0.0.0, and its DatagramChannel
        // only acquires a socket id at bind(), so an unbound sender fails with
        // "send: no socket id". Both are pre-existing gaps unrelated to the
        // registry-lock behaviour under test here.
        int parkedPort = ((InetSocketAddress) parked.getLocalAddress()).getPort();
        t0 = System.nanoTime();
        try (DatagramChannel sender = DatagramChannel.open()) {
            sender.bind(new InetSocketAddress("127.0.0.1", 0));
            sender.send(ByteBuffer.wrap("wake".getBytes()),
                    new InetSocketAddress("127.0.0.1", parkedPort));
        }
        receiver.join(5000);
        System.out.println((receiver.isAlive() ? "FAIL " : "OK   ") + "parked receive woke in "
                + ((System.nanoTime() - t0) / 1_000_000) + "ms");

        parked.close();
        System.out.println("OK   parked channel closed");
        System.out.println("DONE");
        System.exit(0);
    }
}
