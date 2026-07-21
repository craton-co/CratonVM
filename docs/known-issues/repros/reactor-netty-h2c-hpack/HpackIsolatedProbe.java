package io.netty.handler.codec.http2;

import io.netty.buffer.ByteBuf;
import io.netty.buffer.UnpooledByteBufAllocator;

// Package-private access lets us call HpackDecoder directly, bypassing
// sockets, reactor-netty, and the HTTP/2 frame reader entirely. If this
// reproduces the "not enough data" failure purely from feeding it the
// known-good 49-byte HPACK payload, the bug is 100% inside HpackDecoder's
// own state-machine execution on CratonVM -- not buffer/socket bridging
// (already ruled out by ByteBufSliceProbe and RawNioProbe).
public class HpackIsolatedProbe {
    public static void main(String[] args) throws Exception {
        int[] payloadBytes = {
            0x3f, 0xe1, 0x1f, // dynamic table size update -> 4096
            0x83,             // indexed header field, static index 3 (:method: POST)
            0x41,             // literal header w/ incremental indexing, static name index 1 (:authority)
            0x8b,             // huffman flag + length=11
        };
        // 11 bytes of huffman-encoded ":authority" value. We don't have the
        // real captured bytes handy, so use a valid Huffman encoding of an
        // 11-character ASCII string: "localhost:1" encoded would need real
        // huffman tables. Instead, sidestep Huffman entirely and use a
        // NON-huffman literal (clear high bit) so length=11 raw ASCII bytes
        // are trivially valid and self-checking.
        // Redo byte5 without the huffman flag: 0x0b (length=11, H=0).
        payloadBytes[5] = 0x0b;
        byte[] valueBytes = "example.com".getBytes(); // exactly 11 bytes

        ByteBuf outer = UnpooledByteBufAllocator.DEFAULT.directBuffer(2048, 2048);
        try {
            // 9-byte fake frame header (content irrelevant, just consumed
            // and discarded like DefaultHttp2FrameReader does).
            for (int i = 0; i < 9; i++) {
                outer.writeByte(0xF0 + i);
            }
            for (int b : payloadBytes) {
                outer.writeByte(b);
            }
            outer.writeBytes(valueBytes);
            int payloadLen = payloadBytes.length + valueBytes.length;
            System.out.println("payloadLen=" + payloadLen + " (should be 6+11=17 for this simplified payload)");

            // Trailing filler mimicking the DATA frame that follows in the
            // real exchange, so the outer buffer isn't exactly frame-sized.
            for (int i = 0; i < 20; i++) {
                outer.writeByte(0xD0 + (i % 16));
            }

            outer.readBytes(9); // consume the fake frame header, like DefaultHttp2FrameReader
            ByteBuf slice = outer.readSlice(payloadLen);
            System.out.println("slice before decode: readerIndex=" + slice.readerIndex()
                    + " writerIndex=" + slice.writerIndex() + " cap=" + slice.capacity());

            HpackDecoder decoder = new HpackDecoder(8192);
            DefaultHttp2Headers headers = new DefaultHttp2Headers();
            try {
                decoder.decode(1, slice, headers, true);
                System.out.println("DECODE SUCCEEDED: headers=" + headers);
            } catch (Throwable t) {
                System.out.println("DECODE FAILED: " + t);
                t.printStackTrace();
            }
        } finally {
            outer.release();
        }
    }
}
