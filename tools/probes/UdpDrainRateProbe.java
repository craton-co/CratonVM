import java.net.*;
import java.util.concurrent.atomic.AtomicInteger;

/// Isolates the "is the receive loop too slow to drain the OS buffer" hypothesis
/// from docs/known-issues/tomcat/testmulticastpackages-async-send-message-loss-20260925.md:
/// a sender fires N small UDP datagrams back-to-back with no pacing (mirroring
/// TestMulticastPackages.testDataSendASYNCM's NO_ACK burst), a receiver drains
/// with a tight DatagramSocket.receive() loop, and both sides are timed.
/// Compare per-receive cost and total loss % between CratonVM and HotSpot.
///
/// Usage: cratonvm --jdk-only -cp <dir> UdpDrainRateProbe [count] [payloadBytes]
public class UdpDrainRateProbe {
    public static void main(String[] args) throws Exception {
        int count = args.length > 0 ? Integer.parseInt(args[0]) : 10000;
        int payloadBytes = args.length > 1 ? Integer.parseInt(args[1]) : 32;

        DatagramSocket recvSocket = new DatagramSocket(0);
        recvSocket.setReceiveBufferSize(1024 * 1024 * 10);
        recvSocket.setSoTimeout(5000);
        int port = recvSocket.getLocalPort();

        AtomicInteger received = new AtomicInteger(0);
        long[] firstRecvNanos = new long[1];
        long[] lastRecvNanos = new long[1];
        Thread receiver = new Thread(() -> {
            byte[] buf = new byte[2048];
            DatagramPacket p = new DatagramPacket(buf, buf.length);
            try {
                while (received.get() < count) {
                    long t0 = System.nanoTime();
                    recvSocket.receive(p);
                    long t1 = System.nanoTime();
                    if (received.get() == 0) firstRecvNanos[0] = t0;
                    lastRecvNanos[0] = t1;
                    received.incrementAndGet();
                }
            } catch (SocketTimeoutException e) {
                System.out.println("receiver timed out after " + received.get() + "/" + count);
            } catch (Exception e) {
                System.out.println("receiver exception: " + e);
            }
        });
        receiver.start();

        Thread.sleep(200); // let receiver bind/start before sender floods

        DatagramSocket sendSocket = new DatagramSocket();
        sendSocket.setSendBufferSize(1024 * 1024 * 10);
        InetAddress dest = InetAddress.getByName("127.0.0.1");
        byte[] payload = new byte[payloadBytes];
        long sendStart = System.nanoTime();
        for (int i = 0; i < count; i++) {
            DatagramPacket p = new DatagramPacket(payload, payload.length, dest, port);
            sendSocket.send(p);
        }
        long sendEnd = System.nanoTime();
        System.out.println("send phase: " + count + " datagrams in " + (sendEnd - sendStart) / 1_000_000.0 +
                " ms, " + (sendEnd - sendStart) / 1000.0 / count + " us/send");

        receiver.join(6000);
        int got = received.get();
        double lossPct = 100.0 * (count - got) / count;
        System.out.println("received: " + got + "/" + count + " (" + lossPct + "% loss)");
        if (got > 1) {
            double recvWindowMs = (lastRecvNanos[0] - firstRecvNanos[0]) / 1_000_000.0;
            System.out.println("receive window: " + recvWindowMs + " ms, " +
                    (recvWindowMs * 1000.0 / got) + " us/receive avg");
        }
        recvSocket.close();
        sendSocket.close();
        System.out.println("DONE OK");
    }
}
