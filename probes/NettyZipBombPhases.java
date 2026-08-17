import io.netty.buffer.ByteBuf;
import io.netty.buffer.PooledByteBufAllocator;
import io.netty.channel.ChannelHandlerContext;
import io.netty.channel.ChannelInboundHandlerAdapter;
import io.netty.channel.embedded.EmbeddedChannel;
import io.netty.handler.codec.http.*;

/** testZipBomb split into timed phases, with a settable chunk count. */
public final class NettyZipBombPhases {
    static long total;

    static final class Sink extends ChannelInboundHandlerAdapter {
        long total;
        @Override public void channelRead(ChannelHandlerContext ctx, Object msg) {
            PooledByteBufAllocator allocator = (PooledByteBufAllocator) ctx.alloc();
            allocator.metric().usedHeapMemory();
            allocator.metric().usedDirectMemory();
            if (msg instanceof HttpContent) {
                HttpContent buf = (HttpContent) msg;
                total += buf.content().readableBytes();
                buf.release();
            }
        }
    }

    public static void main(String[] args) throws Exception {
        String encoding = args.length > 0 ? args[0] : "gzip";
        int numberOfChunks = args.length > 1 ? Integer.parseInt(args[1]) : 256;
        int chunkSize = 1024 * 1024;

        long t0 = System.nanoTime();
        EmbeddedChannel compressionChannel = new EmbeddedChannel(new HttpContentCompressor());
        DefaultFullHttpRequest req = new DefaultFullHttpRequest(HttpVersion.HTTP_1_1, HttpMethod.GET, "/");
        req.headers().set(HttpHeaderNames.ACCEPT_ENCODING, encoding);
        compressionChannel.writeInbound(req);
        DefaultHttpResponse response = new DefaultHttpResponse(HttpVersion.HTTP_1_1, HttpResponseStatus.OK);
        response.headers().set(HttpHeaderNames.TRANSFER_ENCODING, HttpHeaderValues.CHUNKED);
        compressionChannel.writeOutbound(response);
        long t1 = System.nanoTime();

        for (int i = 0; i < numberOfChunks; i++) {
            ByteBuf buffer = compressionChannel.alloc().buffer(chunkSize);
            buffer.writeZero(chunkSize);
            compressionChannel.writeOutbound(new DefaultHttpContent(buffer));
        }
        compressionChannel.writeOutbound(LastHttpContent.EMPTY_LAST_CONTENT);
        compressionChannel.finish();
        compressionChannel.releaseInbound();
        long t2 = System.nanoTime();

        ByteBuf compressed = compressionChannel.alloc().buffer();
        HttpMessage message = null;
        while (true) {
            HttpObject obj = compressionChannel.readOutbound();
            if (obj == null) { break; }
            if (obj instanceof HttpMessage) { message = (HttpMessage) obj; }
            if (obj instanceof HttpContent) {
                HttpContent content = (HttpContent) obj;
                compressed.writeBytes(content.content());
                content.release();
            }
        }
        long t3 = System.nanoTime();

        PooledByteBufAllocator allocator = new PooledByteBufAllocator(false);
        Sink sink = new Sink();
        EmbeddedChannel decompressChannel = new EmbeddedChannel(new HttpContentDecompressor(0), sink);
        decompressChannel.config().setAllocator(allocator);
        decompressChannel.writeInbound(message);
        decompressChannel.writeInbound(new DefaultLastHttpContent(compressed));
        long t4 = System.nanoTime();

        System.out.printf("%s chunks=%d setup=%dms compress=%dms drain=%dms decompress=%dms total=%dms out=%d%n",
                encoding, numberOfChunks,
                (t1-t0)/1000000, (t2-t1)/1000000, (t3-t2)/1000000, (t4-t3)/1000000, (t4-t0)/1000000,
                sink.total);
    }
}
