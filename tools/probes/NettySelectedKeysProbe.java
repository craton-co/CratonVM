// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import io.netty.bootstrap.Bootstrap;
import io.netty.bootstrap.ServerBootstrap;
import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import io.netty.channel.Channel;
import io.netty.channel.ChannelHandlerContext;
import io.netty.channel.ChannelInboundHandlerAdapter;
import io.netty.channel.ChannelInitializer;
import io.netty.channel.EventLoopGroup;
import io.netty.channel.nio.NioEventLoopGroup;
import io.netty.channel.socket.SocketChannel;
import io.netty.channel.socket.nio.NioServerSocketChannel;
import io.netty.channel.socket.nio.NioSocketChannel;

import java.net.InetSocketAddress;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Makes F4's selected-key fast path actually EXECUTE, which nothing else does.
 *
 * <h2>Why this probe has to exist</h2>
 *
 * `append_selected_keys_fast` engages only for netty's own
 * `io.netty.channel.nio.SelectedSelectionKeySet` — matched on the exact class
 * name, because a subclass may override `add` and only the generic
 * `invoke_virtual` path honours an override. The regression suite has no netty
 * on its classpath, and the JDK selector it does use is served by the on-demand
 * `selectedKeys()` builder rather than the field, so the census there honestly
 * reads `selected-keys fast=0 generic=0`: the path had never run outside its
 * unit tests. Shipping a fast path nothing has executed is worse than shipping
 * none.
 *
 * <h2>The one thing that decides whether this probe proves anything</h2>
 *
 * Netty installs its key set by REFLECTING on `sun.nio.ch.SelectorImpl`'s
 * `selectedKeys` / `publicSelectedKeys` fields. On JDK 16+ that needs
 *
 * <pre>--add-opens java.base/sun.nio.ch=ALL-UNNAMED</pre>
 *
 * Without it netty catches the failure and silently keeps the JDK's own set —
 * and then this probe runs green while exercising nothing, which is precisely
 * the false green it exists to avoid. So it ASSERTS the optimisation is live
 * before sending any traffic, by reading back the selector's field type, and
 * fails loudly if it is not.
 *
 * <h2>Reading the result</h2>
 *
 * <pre>
 *   CRATONVM_SC_IO_STATS=1                  census; the row that matters is
 *                                           `selected-keys fast=N generic=M`
 *   CRATONVM_SEL_FAST_KEYS=0                the off arm — expect fast=0, and
 *                                           identical echo results
 * </pre>
 *
 * `fast=0 generic=0` means the path still never ran and the probe has not done
 * its job. `fast>0` is the first evidence the code executes at all.
 */
public class NettySelectedKeysProbe {

    static final int MESSAGES = 2_000;
    static final int PAYLOAD = 64;

    /** Server side: echo whatever arrives. */
    static final class EchoServerHandler extends ChannelInboundHandlerAdapter {
        @Override
        public void channelRead(ChannelHandlerContext ctx, Object msg) {
            ctx.writeAndFlush(msg);
        }

        @Override
        public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) {
            ctx.close();
        }
    }

    /** Client side: count bytes back, release, and signal when done. */
    static final class CountingClientHandler extends ChannelInboundHandlerAdapter {
        final CountDownLatch done;
        long bytes = 0;
        long sum = 0;

        CountingClientHandler(CountDownLatch done) {
            this.done = done;
        }

        @Override
        public void channelRead(ChannelHandlerContext ctx, Object msg) {
            ByteBuf b = (ByteBuf) msg;
            try {
                bytes += b.readableBytes();
                while (b.isReadable()) {
                    sum += b.readByte() & 0xFF;
                }
            } finally {
                b.release();
            }
            if (bytes >= (long) MESSAGES * PAYLOAD) {
                done.countDown();
            }
        }

        @Override
        public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) {
            cause.printStackTrace();
            ctx.close();
        }
    }

    /**
     * Is netty's own key set actually installed on this event loop's selector?
     *
     * Read back through the live `Selector` rather than trusting the absence of
     * a warning: netty's fallback is silent by design, and a probe that assumed
     * success would report a green run for a path that never executed.
     */
    static String installedKeySetType(Channel ch) throws Exception {
        // The event loop's selector is private; reach it the same way netty's
        // own tests do. Any failure here is reported, not swallowed.
        Object loop = ch.eventLoop();
        java.lang.reflect.Field selField = null;
        for (Class<?> c = loop.getClass(); c != null; c = c.getSuperclass()) {
            try {
                selField = c.getDeclaredField("selector");
                break;
            } catch (NoSuchFieldException ignored) {
                // keep walking
            }
        }
        if (selField == null) {
            return "UNKNOWN(no selector field)";
        }
        selField.setAccessible(true);
        Object sel = selField.get(loop);
        if (sel == null) {
            return "UNKNOWN(null selector)";
        }
        java.lang.reflect.Field keys = null;
        for (Class<?> c = sel.getClass(); c != null; c = c.getSuperclass()) {
            try {
                keys = c.getDeclaredField("selectedKeys");
                break;
            } catch (NoSuchFieldException ignored) {
                // keep walking
            }
        }
        if (keys == null) {
            return "UNKNOWN(no selectedKeys field on " + sel.getClass().getName() + ")";
        }
        keys.setAccessible(true);
        Object set = keys.get(sel);
        return set == null ? "null" : set.getClass().getName();
    }

    public static void main(String[] args) throws Exception {
        EventLoopGroup boss = new NioEventLoopGroup(1);
        EventLoopGroup worker = new NioEventLoopGroup(1);
        EventLoopGroup clientGroup = new NioEventLoopGroup(1);
        try {
            ServerBootstrap sb = new ServerBootstrap();
            sb.group(boss, worker)
                    .channel(NioServerSocketChannel.class)
                    .childHandler(new ChannelInitializer<SocketChannel>() {
                        @Override
                        protected void initChannel(SocketChannel ch) {
                            ch.pipeline().addLast(new EchoServerHandler());
                        }
                    });
            Channel server = sb.bind(new InetSocketAddress("127.0.0.1", 0)).sync().channel();
            int port = ((InetSocketAddress) server.localAddress()).getPort();

            CountDownLatch done = new CountDownLatch(1);
            CountingClientHandler client = new CountingClientHandler(done);
            Bootstrap cb = new Bootstrap();
            cb.group(clientGroup)
                    .channel(NioSocketChannel.class)
                    .handler(new ChannelInitializer<SocketChannel>() {
                        @Override
                        protected void initChannel(SocketChannel ch) {
                            ch.pipeline().addLast(client);
                        }
                    });
            Channel conn = cb.connect(new InetSocketAddress("127.0.0.1", port)).sync().channel();

            String keySet = installedKeySetType(conn);
            System.out.println("CK netty selectedKeys impl = " + keySet);
            boolean optimised = keySet.contains("SelectedSelectionKeySet");
            System.out.println("CK netty key-set optimisation = " + (optimised ? "LIVE" : "ABSENT"));

            byte[] payload = new byte[PAYLOAD];
            for (int i = 0; i < PAYLOAD; i++) {
                payload[i] = (byte) i;
            }
            long t0 = System.nanoTime();
            for (int i = 0; i < MESSAGES; i++) {
                conn.writeAndFlush(Unpooled.wrappedBuffer(payload.clone()));
            }
            boolean finished = done.await(60, TimeUnit.SECONDS);
            long elapsed = System.nanoTime() - t0;

            System.out.println("CK netty echoed_bytes = " + client.bytes);
            System.out.println("CK netty payload_sum = " + client.sum);
            System.out.println("CK netty complete = " + finished);
            System.out.printf("netty %d msgs of %dB in %.1f ms%n",
                    MESSAGES, PAYLOAD, elapsed / 1e6);

            conn.close().sync();
            server.close().sync();

            if (!optimised) {
                // Loud, not a warning: without the key set installed this probe
                // exercises the generic path and proves nothing about F4.
                System.out.println("FAIL netty key-set optimisation not installed — "
                        + "rerun with --add-opens java.base/sun.nio.ch=ALL-UNNAMED");
                System.exit(2);
            }
            if (!finished || client.bytes != (long) MESSAGES * PAYLOAD) {
                System.out.println("FAIL netty echo incomplete");
                System.exit(3);
            }
            System.out.println("PASS NettySelectedKeysProbe");
        } finally {
            boss.shutdownGracefully();
            worker.shutdownGracefully();
            clientGroup.shutdownGracefully();
        }
    }
}
