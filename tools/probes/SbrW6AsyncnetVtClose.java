// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// SBR wave 6, lane asyncnet: a virtual thread blocked in a real SocketChannel.read; the main thread closes the
// channel (default) or interrupts the thread (any second argument). Prints a histogram over N runs.
//   java SbrW6AsyncnetVtClose 25 [i]
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

public class SbrW6AsyncnetVtClose {
    static String one(boolean interrupt) throws Exception {
        try (ServerSocketChannel s = ServerSocketChannel.open()) {
            s.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            SocketChannel c = SocketChannel.open(s.getLocalAddress());
            try (SocketChannel a = s.accept()) {
                AtomicReference<String> res = new AtomicReference<>("never");
                CountDownLatch started = new CountDownLatch(1);
                Thread v = Thread.ofVirtual().start(() -> {
                    started.countDown();
                    try {
                        c.read(ByteBuffer.allocate(4));
                        res.set("read-returned");
                    } catch (Throwable e) {
                        res.set(e.getClass().getSimpleName());
                    }
                });
                started.await();
                Thread.sleep(150);
                if (interrupt) v.interrupt(); else c.close();
                v.join(5000);
                String r = res.get() + " alive=" + v.isAlive();
                if (v.isAlive()) { c.close(); v.join(2000); }
                return r;
            }
        }
    }
    public static void main(String[] a) throws Exception {
        int n = Integer.parseInt(a[0]);
        boolean intr = a.length > 1;
        java.util.Map<String,Integer> m = new java.util.TreeMap<>();
        for (int i = 0; i < n; i++) m.merge(one(intr), 1, Integer::sum);
        System.out.println((intr ? "interrupt " : "close ") + m);
    }
}
