// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.*;
import java.nio.charset.StandardCharsets;
import java.util.Iterator;
import java.util.Set;

/**
 * Tomcat's NIO endpoint is level-triggered: it registers OP_READ, and after it
 * has consumed a request it expects the selector to STOP reporting the socket
 * readable until more bytes arrive. QuartzEndpointWebIntegrationTests dispatches
 * a single client POST ~23,700 times server-side, which is what a selector that
 * keeps re-reporting readiness (or a read that does not drain) produces.
 *
 * One client sends exactly ONE request and then goes quiet. The server counts
 * how many times the selector wakes it for that channel and how many bytes each
 * read returns. A conforming VM: one readable event carrying the request, then
 * silence (and a single -1 at close).
 */
public class SelectorReadinessProbe {
    public static void main(String[] a) throws Exception {
        int budgetMs = a.length > 0 ? Integer.parseInt(a[0]) : 4000;
        ServerSocketChannel ssc = ServerSocketChannel.open();
        ssc.bind(new InetSocketAddress("127.0.0.1", 0));
        int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();
        ssc.configureBlocking(false);
        Selector sel = Selector.open();
        ssc.register(sel, SelectionKey.OP_ACCEPT);

        Thread client = new Thread(() -> {
            try (SocketChannel c = SocketChannel.open(new InetSocketAddress("127.0.0.1", port))) {
                c.write(ByteBuffer.wrap("PING\n".getBytes(StandardCharsets.UTF_8)));
                Thread.sleep(budgetMs + 1500);   // send once, then stay quiet and connected
            } catch (Exception e) { }
        });
        client.setDaemon(true);
        client.start();

        long deadline = System.currentTimeMillis() + budgetMs;
        long wakeups = 0, reads = 0, zeroReads = 0, bytes = 0, eof = 0;
        ByteBuffer buf = ByteBuffer.allocate(256);
        while (System.currentTimeMillis() < deadline) {
            if (sel.select(100) == 0) continue;
            Set<SelectionKey> keys = sel.selectedKeys();
            for (Iterator<SelectionKey> it = keys.iterator(); it.hasNext(); ) {
                SelectionKey k = it.next();
                it.remove();
                if (k.isAcceptable()) {
                    SocketChannel ch = ((ServerSocketChannel) k.channel()).accept();
                    if (ch != null) { ch.configureBlocking(false); ch.register(sel, SelectionKey.OP_READ); }
                } else if (k.isReadable()) {
                    wakeups++;
                    buf.clear();
                    int n;
                    try { n = ((SocketChannel) k.channel()).read(buf); }
                    catch (IOException e) { k.cancel(); continue; }
                    reads++;
                    if (n > 0) bytes += n;
                    else if (n == 0) zeroReads++;
                    else { eof++; k.cancel(); }
                }
            }
        }
        System.out.println("readableWakeups=" + wakeups + " reads=" + reads
            + " bytesTotal=" + bytes + " zeroByteReads=" + zeroReads + " eof=" + eof);
        boolean ok = wakeups <= 5 && zeroReads <= 5 && bytes == 5;
        System.out.println(ok ? "OK" : "FAIL: selector kept reporting a drained socket readable");
        System.exit(ok ? 0 : 1);
    }
}
