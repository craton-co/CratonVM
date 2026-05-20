#!/usr/bin/env bash
# RI.11 — Netty 4 echo server + client loopback.

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
V="${NETTY_VERSION:-4.1.110.Final}"

for m in netty-common netty-buffer netty-transport netty-codec netty-handler netty-resolver; do
    smoke_download "$MVN_CENTRAL/io/netty/$m/$V/$m-$V.jar" "$FIXTURE_CACHE/$m-$V.jar"
done

CP=$(ls "$FIXTURE_CACHE"/netty-*.jar | tr '\n' ':')
FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR"

cat > "$FIX_DIR/NettyEchoSmoke.java" <<'JAVA'
import io.netty.bootstrap.*;
import io.netty.buffer.ByteBuf;
import io.netty.channel.*;
import io.netty.channel.nio.NioEventLoopGroup;
import io.netty.channel.socket.SocketChannel;
import io.netty.channel.socket.nio.*;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;

public class NettyEchoSmoke {
    public static void main(String[] args) throws Exception {
        NioEventLoopGroup g = new NioEventLoopGroup(1);
        try {
            ServerBootstrap sb = new ServerBootstrap();
            sb.group(g).channel(NioServerSocketChannel.class)
              .childHandler(new ChannelInitializer<SocketChannel>() {
                  protected void initChannel(SocketChannel ch) {
                      ch.pipeline().addLast(new ChannelInboundHandlerAdapter() {
                          public void channelRead(ChannelHandlerContext ctx, Object msg) {
                              ctx.writeAndFlush(msg);
                          }
                      });
                  }
              });
            Channel ss = sb.bind(0).sync().channel();
            int port = ((InetSocketAddress) ss.localAddress()).getPort();

            Bootstrap cb = new Bootstrap();
            final StringBuilder got = new StringBuilder();
            final Object lock = new Object();
            cb.group(g).channel(NioSocketChannel.class)
              .handler(new ChannelInitializer<SocketChannel>() {
                  protected void initChannel(SocketChannel ch) {
                      ch.pipeline().addLast(new ChannelInboundHandlerAdapter() {
                          public void channelRead(ChannelHandlerContext ctx, Object msg) {
                              ByteBuf b = (ByteBuf) msg;
                              got.append(b.toString(StandardCharsets.UTF_8));
                              b.release();
                              synchronized (lock) { lock.notify(); }
                          }
                      });
                  }
              });
            Channel c = cb.connect("127.0.0.1", port).sync().channel();
            c.writeAndFlush(io.netty.buffer.Unpooled.copiedBuffer("NETTY_OK", StandardCharsets.UTF_8)).sync();
            synchronized (lock) { lock.wait(5000); }
            c.close().sync();
            ss.close().sync();
            System.out.println("NETTY_ECHO_GOT=" + got);
        } finally {
            g.shutdownGracefully().sync();
        }
    }
}
JAVA

"$JAVA_HOME_FOR_SMOKE/bin/javac" -cp "$CP" -d "$FIX_DIR" "$FIX_DIR/NettyEchoSmoke.java"

SMOKE_TIMEOUT=180 smoke_run_cratonvm --Xmx 512m --classpath "$FIX_DIR:$CP" -- NettyEchoSmoke

smoke_require_signal "NETTY_ECHO_GOT=NETTY_OK"
