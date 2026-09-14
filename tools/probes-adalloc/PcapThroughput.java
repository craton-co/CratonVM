// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Reduction of io.netty.handler.pcap.PcapWriteHandlerTest.writePcapGreaterThan4Gb
// to a timed steady-state loop, for docs/known-issues/perf/
// perf-netty-adaptive-allocator-throughput-20260817.md.
//
// Same handler chain, same 65 495-byte chunk, same
// writeInbound(payload.retainedDuplicate()) as the test; the payload bytes are
// discarded by a counting OutputStream so the loop measures the netty side and
// not the I/O.
//
// -Diters=N sets the timed iteration count; the loop is warmed with iters/4
// (capped) first so a reported per-iteration figure is steady state, not
// class-loading.
import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import io.netty.channel.ChannelHandlerContext;
import io.netty.channel.ChannelInboundHandlerAdapter;
import io.netty.channel.ChannelOutboundHandlerAdapter;
import io.netty.channel.ChannelPromise;
import io.netty.channel.embedded.EmbeddedChannel;
import io.netty.handler.pcap.PcapWriteHandler;
import io.netty.util.ReferenceCountUtil;

import java.io.OutputStream;
import java.net.InetSocketAddress;

public final class PcapThroughput {

    static final class CountingOutputStream extends OutputStream {
        long bytesWritten;

        @Override
        public void write(int b) {
            bytesWritten++;
        }

        @Override
        public void write(byte[] b, int off, int len) {
            bytesWritten += len;
        }
    }

    static final class DiscardWrites extends ChannelOutboundHandlerAdapter {
        @Override
        public void write(ChannelHandlerContext ctx, Object msg, ChannelPromise promise) {
            ReferenceCountUtil.release(msg);
            promise.setSuccess();
        }

        @Override
        public void flush(ChannelHandlerContext ctx) {
            // discard
        }
    }

    static final class DiscardReads extends ChannelInboundHandlerAdapter {
        @Override
        public void channelRead(ChannelHandlerContext ctx, Object msg) {
            ReferenceCountUtil.release(msg);
        }
    }

    public static void main(String[] args) throws Exception {
        int iters = Integer.getInteger("iters", 2000);
        int warm = Math.min(iters, Math.max(200, iters / 4));

        InetSocketAddress serverAddr = new InetSocketAddress("1.1.1.1", 1234);
        InetSocketAddress clientAddr = new InetSocketAddress("2.2.2.2", 3456);

        CountingOutputStream out = new CountingOutputStream();
        EmbeddedChannel ch = new EmbeddedChannel(
                new DiscardWrites(),
                PcapWriteHandler.builder()
                        .forceTcpChannel(serverAddr, clientAddr, true)
                        .build(out),
                new DiscardReads());

        int chunkSize = 0xFFFF - 40;
        byte[] raw = new byte[chunkSize];
        java.util.Arrays.fill(raw, (byte) 'X');
        ByteBuf payload = Unpooled.wrappedBuffer(raw);

        try {
            for (int i = 0; i < warm; i++) {
                ch.writeInbound(payload.retainedDuplicate());
            }
            long before = out.bytesWritten;
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                ch.writeInbound(payload.retainedDuplicate());
            }
            long dt = System.nanoTime() - t0;
            long bytes = out.bytesWritten - before;
            double perIter = dt / (double) iters / 1000.0;
            double mbps = bytes / (dt / 1e9) / (1024.0 * 1024.0);
            System.out.printf("PcapThroughput iters=%d per_iter_us=%.1f throughput_MBps=%.1f%n",
                    iters, perIter, mbps);
        } finally {
            payload.release();
            ch.finishAndReleaseAll();
        }
    }
}
