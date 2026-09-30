import java.io.File;
import java.io.RandomAccessFile;
import java.net.*;
import java.nio.channels.*;

/**
 * Census v3 — the shape that actually reproduces: a small delegating method
 * (netty's AbstractNioChannel.isOpen()) called hot enough to be COMPILED, with
 * the JDK predicate invoked from inside that compiled body.
 *
 * v1 (method references) and v2 (predicate called straight from an OSR loop)
 * both read clean on a VM that is not: only a call issued from a fully
 * compiled callee body takes the path that bypasses the native override.
 */
public class ChannelStateAfterCloseCensus {
    static final int REPS = Integer.getInteger("reps", 400000);

    static boolean askChannel(Channel c)            { return c.isOpen(); }
    static boolean askSelectable(SelectableChannel c){ return c.isOpen(); }
    static boolean askAIC(java.nio.channels.spi.AbstractInterruptibleChannel c){ return c.isOpen(); }
    static boolean askKeyValid(SelectionKey k)      { return k.isValid(); }
    static boolean askSelector(Selector s)          { return s.isOpen(); }
    static boolean askSockClosed(Socket s)          { return s.isClosed(); }
    static boolean askSockConnected(Socket s)       { return s.isConnected(); }
    static boolean askServerSockClosed(ServerSocket s){ return s.isClosed(); }

    static int wrongAt;

    static void report(String what, boolean expected, boolean first, int wrong) {
        StringBuilder b = new StringBuilder(what);
        while (b.length() < 34) { b.append(' '); }
        System.out.println(b + " expected=" + expected + " first=" + first
                + " wrong=" + wrong + "/" + REPS + " firstWrongAt=" + wrongAt);
    }

    static int runChannel(Channel c, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askChannel(c) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }
    static int runSelectable(SelectableChannel c, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askSelectable(c) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }
    static int runAIC(java.nio.channels.spi.AbstractInterruptibleChannel c, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askAIC(c) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }
    static int runKey(SelectionKey k, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askKeyValid(k) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }
    static int runSelector(Selector s, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askSelector(s) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }
    static int runSockClosed(Socket s, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askSockClosed(s) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }
    static int runSockConnected(Socket s, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askSockConnected(s) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }
    static int runServerSockClosed(ServerSocket s, boolean exp) {
        int w = 0; wrongAt = -1;
        for (int i = 0; i < REPS; i++) { if (askServerSockClosed(s) != exp) { w++; if (wrongAt<0) wrongAt=i; } }
        return w;
    }

    public static void main(String[] a) throws Exception {
        SocketChannel sc = SocketChannel.open();
        sc.close();
        report("SocketChannel.isOpen(closed)", false, sc.isOpen(), runSelectable(sc, false));

        SocketChannel sc2 = SocketChannel.open();
        sc2.close();
        report("SocketChannel.isOpen(Channel)", false, sc2.isOpen(), runChannel(sc2, false));
        SocketChannel sc3 = SocketChannel.open();
        sc3.close();
        report("SocketChannel.isOpen(AIC)", false, sc3.isOpen(), runAIC(sc3, false));

        SocketChannel scLive = SocketChannel.open();
        report("SocketChannel.isOpen(live)", true, scLive.isOpen(), runSelectable(scLive, true));
        scLive.close();

        ServerSocketChannel ssc = ServerSocketChannel.open();
        ssc.bind(new InetSocketAddress("127.0.0.1", 0));
        Selector sel = Selector.open();
        ssc.configureBlocking(false);
        SelectionKey key = ssc.register(sel, SelectionKey.OP_ACCEPT);
        report("SelectionKey.isValid(live)", true, key.isValid(), runKey(key, true));
        key.cancel();
        sel.selectNow();
        report("SelectionKey.isValid(cancelled)", false, key.isValid(), runKey(key, false));

        ssc.close();
        report("ServerSocketChannel.isOpen", false, ssc.isOpen(), runSelectable(ssc, false));

        sel.close();
        report("Selector.isOpen(closed)", false, sel.isOpen(), runSelector(sel, false));

        DatagramChannel dc = DatagramChannel.open();
        dc.close();
        report("DatagramChannel.isOpen", false, dc.isOpen(), runSelectable(dc, false));

        Pipe pipe = Pipe.open();
        pipe.source().close();
        report("Pipe.SourceChannel.isOpen", false, pipe.source().isOpen(), runSelectable(pipe.source(), false));
        pipe.sink().close();
        report("Pipe.SinkChannel.isOpen", false, pipe.sink().isOpen(), runSelectable(pipe.sink(), false));
        report("Pipe.SourceChannel.isOpen(AIC)", false, pipe.source().isOpen(), runAIC(pipe.source(), false));
        report("Pipe.SinkChannel.isOpen(AIC)", false, pipe.sink().isOpen(), runAIC(pipe.sink(), false));

        File tmp = File.createTempFile("chanstate", ".bin");
        tmp.deleteOnExit();
        RandomAccessFile raf = new RandomAccessFile(tmp, "rw");
        FileChannel fc = raf.getChannel();
        fc.close();
        report("FileChannel.isOpen(closed)", false, fc.isOpen(), runChannel(fc, false));
        report("FileChannel.isOpen(AIC)", false, fc.isOpen(), runAIC(fc, false));
        raf.close();

        ServerSocket ss = new ServerSocket(0);
        Socket cli = new Socket("127.0.0.1", ss.getLocalPort());
        Socket acc = ss.accept();
        report("Socket.isConnected(live)", true, cli.isConnected(), runSockConnected(cli, true));
        report("Socket.isClosed(live)", false, cli.isClosed(), runSockClosed(cli, false));
        cli.close();
        report("Socket.isClosed(closed)", true, cli.isClosed(), runSockClosed(cli, true));
        report("Socket.isConnected(closed)", true, cli.isConnected(), runSockConnected(cli, true));
        acc.close();
        ss.close();
        report("ServerSocket.isClosed(closed)", true, ss.isClosed(), runServerSockClosed(ss, true));
    }
}
