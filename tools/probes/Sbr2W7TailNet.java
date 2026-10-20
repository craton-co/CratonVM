import java.io.*;
import java.net.*;
import java.nio.channels.*;

/** SBR2 W7 TAIL: connection-failure exception classes and messages (Socket, SocketChannel, ServerSocket). */
public class Sbr2W7TailNet {
    static void row(String n, Throwable t) {
        System.out.println(n + " = " + (t == null ? "no exception" : t.getClass().getName() + ": " + t.getMessage()));
    }
    static int deadPort() throws Exception {
        try (ServerSocket s = new ServerSocket(0)) { return s.getLocalPort(); }
    }
    public static void main(String[] a) throws Exception {
        int p = deadPort();
        try (Socket s = new Socket()) { s.connect(new InetSocketAddress("127.0.0.1", p)); row("socket.connect", null); }
        catch (Throwable t) { row("socket.connect", t); }
        try (Socket s = new Socket()) { s.connect(new InetSocketAddress("127.0.0.1", p), 2000); row("socket.connect.timeout", null); }
        catch (Throwable t) { row("socket.connect.timeout", t); }
        try (Socket s = new Socket("127.0.0.1", p)) { row("socket.ctor", null); }
        catch (Throwable t) { row("socket.ctor", t); }
        try (SocketChannel c = SocketChannel.open()) { c.connect(new InetSocketAddress("127.0.0.1", p)); row("sc.connect", null); }
        catch (Throwable t) { row("sc.connect", t); }
        try (SocketChannel c = SocketChannel.open()) {
            c.configureBlocking(false);
            boolean done = c.connect(new InetSocketAddress("127.0.0.1", p));
            long end = System.currentTimeMillis() + 3000;
            while (!done && System.currentTimeMillis() < end) { done = c.finishConnect(); Thread.sleep(5); }
            row("sc.nonblocking", null);
        } catch (Throwable t) { row("sc.nonblocking", t); }
        try (ServerSocket s1 = new ServerSocket(0); ServerSocket s2 = new ServerSocket(s1.getLocalPort())) { row("ss.inuse", null); }
        catch (Throwable t) { row("ss.inuse", t); }
        try (ServerSocketChannel s1 = ServerSocketChannel.open().bind(new InetSocketAddress("127.0.0.1", 0));
             ServerSocketChannel s2 = ServerSocketChannel.open().bind(s1.getLocalAddress())) { row("ssc.inuse", null); }
        catch (Throwable t) { row("ssc.inuse", t); }
        try (Socket s = new Socket()) { s.bind(new InetSocketAddress("203.0.113.9", 0)); row("socket.bind.unavail", null); }
        catch (Throwable t) { row("socket.bind.unavail", t); }
        try (DatagramSocket d = new DatagramSocket(new InetSocketAddress("203.0.113.9", 0))) { row("dgram.bind.unavail", null); }
        catch (Throwable t) { row("dgram.bind.unavail", t); }
    }
}
