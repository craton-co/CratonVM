// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio20 wave 3, lane L4: the optimizing tier's `ScopedMemoryAccess` fold
// (`jit/src/ir_lower.rs` `lower_scoped_raw`) after its checks were tightened:
// one compare for "primitive array", the null-session / receiver null tests
// left out where the graph answers them, a constant `bigEndian` folded.
//
// Two halves:
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes). Every
//          ByteBuffer width (short/char/int/long/float/double) over a heap
//          buffer in both byte orders, at every offset up to the last in-bounds
//          one, through absolute and relative accessors; out-of-bounds and
//          read-only failures; a direct buffer (the off-heap arm); heap buffers
//          backed by MemorySegment.ofArray(byte[]) in both orders (a NON-null
//          session: the fold deopts or is refused and the real method runs);
//          HotSpot's refusal of an int[] segment view; a closed confined
//          arena's view buffer. Each check runs REPS times so the optimizing
//          tier compiles it; every line catches and prints an unexpected throw.
//   rows   `<row> <N> ns/op` lines: heapbb (16 putInt + 16 getInt over a heap
//          ByteBuffer, R17NioRows' loop), heaple (the same, little-endian),
//          heaplong (putLong/getLong), segbb (getInt over a segment-backed heap
//          view: the non-null-session cost W2L1-1 prices).
//
//   java Nio20ScopedFold conf
//   java -Dnio20.rows=heapbb,heaple,heaplong,segbb Nio20ScopedFold rows
//   java -Dnio20.rows=segbb,segdirect,direct Nio20ScopedFold rows   (wave 4: the
//          non-null-session fold; A/B `CRATONVM_JIT_IR_SCOPED_SESSION_FOLD=0`)
//   java -Dnio20.rows=confined,direct Nio20ScopedFold rows   (wave 5: the
//          confined-session arm; A/B `CRATONVM_JIT_IR_SCOPED_CONFINED_FOLD=0`;
//          `conf` gained confined sweep / other-thread / owner-again / closed)
//
// A/B (cratonvm): build before/after this wave; `CRATONVM_JIT_IR_SCOPED_FOLD=0`
// is the helper-route control. Under CRATONVM_DBG_JITC=1 the fold's sites print
// `unbox accessors: lowered_inline=`.

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class Nio20ScopedFold {
    static final int REPS = 20_000;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio20.rows", "heapbb,heaple,heaplong,segbb"));
        } else {
            conf();
        }
    }

    // ---------------------------------------------------------------- conf

    static String exc(Throwable t) {
        return t.getClass().getName() + ":" + t.getMessage();
    }

    /// Every width at every in-bounds absolute offset, folded into one hash.
    static long sweep(ByteBuffer b) {
        long h = 17;
        int cap = b.capacity();
        for (int i = 0; i + 8 <= cap; i++) {
            b.putLong(i, 0x0102030405060708L * (i + 1));
            h = h * 31 + b.getLong(i);
            b.putInt(i, 0x11223344 ^ i);
            h = h * 31 + b.getInt(i);
            b.putShort(i, (short) (0x8001 + i));
            h = h * 31 + b.getShort(i);
            b.putChar(i, (char) (0xFFF0 - i));
            h = h * 31 + b.getChar(i);
            b.putFloat(i, i * 1.5f);
            h = h * 31 + Float.floatToRawIntBits(b.getFloat(i));
            b.putDouble(i, i * -2.25);
            h = h * 31 + Double.doubleToRawLongBits(b.getDouble(i));
        }
        // the last in-bounds offset of each width
        h = h * 31 + b.getLong(cap - 8) + b.getInt(cap - 4) + b.getShort(cap - 2) + b.getChar(cap - 2);
        return h;
    }

    /// Relative accessors: fill, flip, drain. One record is 4+8+2+2+1 = 17
    /// bytes; three fit a 64-byte buffer.
    static long relative(ByteBuffer b) {
        b.clear();
        for (int k = 0; k < 3; k++) {
            b.putInt(k * 7 + 1).putLong(~(long) k).putShort((short) k).putChar((char) ('a' + k)).put((byte) k);
        }
        b.flip();
        long h = b.remaining();
        while (b.remaining() >= 17) {
            h = h * 31 + b.getInt();
            h = h * 31 + b.getLong();
            h = h * 31 + b.getShort();
            h = h * 31 + b.getChar();
            h = h * 31 + b.get();
        }
        return h;
    }

    static long repeat(java.util.function.LongSupplier f) {
        long h = 0;
        for (int r = 0; r < REPS / 100; r++) {
            h = h * 1_000_003 + f.getAsLong();
        }
        return h;
    }

    /// Print `label value`, or `label threw <class>` -- every conf line goes
    /// through here so one unexpected throw cannot end the run.
    static void line(String label, java.util.function.Supplier<Object> f) {
        String out;
        try {
            out = String.valueOf(f.get());
        } catch (Throwable t) {
            out = "threw " + t.getClass().getName();
        }
        System.out.println(label + " " + out);
    }

    static void conf() {
        ByteBuffer be = ByteBuffer.allocate(64);
        ByteBuffer le = ByteBuffer.allocate(64).order(ByteOrder.LITTLE_ENDIAN);
        line("heap-be sweep", () -> repeat(() -> sweep(be)));
        line("heap-le sweep", () -> repeat(() -> sweep(le)));
        line("heap-be relative", () -> repeat(() -> relative(be)));
        line("heap-le relative", () -> repeat(() -> relative(le)));

        // A slice: a non-zero array offset under the same `hb`.
        ByteBuffer slice = ByteBuffer.allocate(80).position(13).slice();
        line("slice sweep", () -> repeat(() -> sweep(slice)));

        ByteBuffer direct = ByteBuffer.allocateDirect(64);
        line("direct sweep", () -> repeat(() -> sweep(direct)));
        // nio20 wave 4: a direct view over a `GlobalSession$HeapSession` (non-null,
        // never-closing session; the fold admits it inline since wave 4).
        line("direct-view sweep", () -> {
            ByteBuffer directView = MemorySegment.ofBuffer(ByteBuffer.allocateDirect(64)).asByteBuffer();
            return repeat(() -> sweep(directView)) + " direct=" + directView.isDirect();
        });

        // Out of bounds, past warm-up. Each expected failure prints its class
        // and message (deterministic in the JDK's Preconditions / Buffer code).
        line("oob", () -> {
            be.clear(); // `relative` left limit at 51
            for (int r = 0; r < REPS; r++) {
                be.getInt(r & 31);
            }
            String[] oob = new String[4];
            try { oob[0] = "no-throw " + be.getInt(61); } catch (Throwable t) { oob[0] = exc(t); }
            try { be.putLong(57, 1L); oob[1] = "no-throw"; } catch (Throwable t) { oob[1] = exc(t); }
            try { oob[2] = "no-throw " + be.getShort(-1); } catch (Throwable t) { oob[2] = exc(t); }
            be.position(62);
            try { oob[3] = "no-throw " + be.getInt(); } catch (Throwable t) { oob[3] = exc(t); }
            be.clear();
            return String.join(" | ", oob);
        });

        line("read-only", () -> {
            ByteBuffer ro = ByteBuffer.allocate(16).asReadOnlyBuffer();
            String roMsg;
            try { ro.putInt(0, 1); roMsg = "no-throw"; } catch (Throwable t) { roMsg = exc(t); }
            return roMsg + " get=" + ro.getInt(4);
        });

        // A heap segment over a byte[]: a NON-null session (the fold deopts or
        // is refused there; the real method runs).
        byte[] bytes = new byte[64];
        line("seg-byte sweep", () -> {
            ByteBuffer viaBytes = MemorySegment.ofArray(bytes).asByteBuffer();
            return repeat(() -> sweep(viaBytes)) + " bytes[3]=" + bytes[3];
        });
        line("seg-byte-le sweep", () -> {
            ByteBuffer viaBytes = MemorySegment.ofArray(bytes).asByteBuffer().order(ByteOrder.LITTLE_ENDIAN);
            return repeat(() -> sweep(viaBytes)) + " bytes[5]=" + bytes[5];
        });
        // HotSpot refuses a buffer view of a non-byte[] heap segment; the
        // message is part of the conformance surface.
        line("seg-int view", () -> {
            try {
                return "no-throw " + MemorySegment.ofArray(new int[16]).asByteBuffer().capacity();
            } catch (Throwable t) {
                return exc(t);
            }
        });

        // A closed confined arena's view buffer.
        Arena arena = Arena.ofConfined();
        ByteBuffer arenaBuf = arena.allocate(64).asByteBuffer();
        line("arena sweep", () -> repeat(() -> sweep(arenaBuf)));
        arena.close();
        line("arena closed", () -> {
            try {
                return "no-throw " + arenaBuf.getInt(0);
            } catch (Throwable t) {
                return exc(t);
            }
        });

        // nio20 wave 5, lane L2: the confined arm of the fold. The same compiled
        // `sweep` is first made hot on the owner thread over a confined arena's
        // view, then called from ANOTHER thread (WrongThreadException, every
        // time), then on the owner again (still served), then after close.
        Arena conf2 = Arena.ofConfined();
        ByteBuffer confBuf = conf2.allocate(64).asByteBuffer();
        line("confined sweep", () -> repeat(() -> sweep(confBuf)));
        line("confined other-thread", () -> {
            String[] out = new String[2];
            Thread t = new Thread(() -> {
                for (int k = 0; k < 2; k++) {
                    try {
                        out[k] = "no-throw " + sweep(confBuf);
                    } catch (Throwable e) {
                        out[k] = exc(e);
                    }
                }
            });
            t.start();
            try {
                t.join();
            } catch (InterruptedException e) {
                return "interrupted";
            }
            return out[0] + " | " + out[1];
        });
        line("confined owner-again", () -> repeat(() -> sweep(confBuf)));
        conf2.close();
        line("confined closed", () -> {
            try {
                return "no-throw " + sweep(confBuf);
            } catch (Throwable t) {
                return exc(t);
            }
        });
    }

    // ---------------------------------------------------------------- rows

    static final ByteBuffer HB = ByteBuffer.allocate(256);
    static final ByteBuffer HL = ByteBuffer.allocate(256).order(ByteOrder.LITTLE_ENDIAN);
    static final ByteBuffer SEG = MemorySegment.ofArray(new byte[256]).asByteBuffer();
    // nio20 wave 4, lane L2: a DIRECT view whose session is the JDK's never-closing
    // `GlobalSession$HeapSession` (`MemorySegment.ofBuffer` -> `bufferScope` ->
    // `MemorySessionImpl.createHeap`), i.e. the off-heap arm of the fold with a
    // non-null session; and a plain direct buffer (null session) as its control.
    // Holders, so a VM that cannot build one of these fails only its own row.
    static final class SegDirect {
        static final ByteBuffer B = MemorySegment.ofBuffer(ByteBuffer.allocateDirect(256)).asByteBuffer();
    }
    static final class Direct {
        static final ByteBuffer B = ByteBuffer.allocateDirect(256);
    }
    // nio20 wave 5, lane L2: a confined arena's view (owner = this thread), the
    // confined arm of the fold; A/B `CRATONVM_JIT_IR_SCOPED_CONFINED_FOLD=0`.
    static final class Confined {
        static final ByteBuffer B = Arena.ofConfined().allocate(256).asByteBuffer();
    }

    static int confinedBb(ByteBuffer b, int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * ((i + k) & 31));
        return h;
    }

    static int heapBb(ByteBuffer b, int i) {
        b.clear();
        for (int k = 0; k < 16; k++) b.putInt(i + k);
        b.flip();
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt();
        return h;
    }

    static int heapLong(ByteBuffer b, int i) {
        b.clear();
        for (int k = 0; k < 16; k++) b.putLong(i + k);
        b.flip();
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + (int) b.getLong();
        return h;
    }

    static int segBb(ByteBuffer b, int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * ((i + k) & 31));
        return h;
    }

    // Copies of `segBb`, one per row, so each row's receiver profile stays monomorphic.
    static int segDirectBb(ByteBuffer b, int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * ((i + k) & 31));
        return h;
    }

    static int directBb(ByteBuffer b, int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * ((i + k) & 31));
        return h;
    }

    static void rows(String which) {
        int n = Integer.getInteger("nio20.n", 200_000);
        for (String row : which.split(",")) {
            int sink = 0;
            for (int warm = 0; warm < 3; warm++) {
                long t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    switch (row) {
                        case "heapbb" -> sink += heapBb(HB, i);
                        case "heaple" -> sink += heapBb(HL, i);
                        case "heaplong" -> sink += heapLong(HB, i);
                        case "segbb" -> sink += segBb(SEG, i);
                        case "segdirect" -> sink += segDirectBb(SegDirect.B, i);
                        case "direct" -> sink += directBb(Direct.B, i);
                        case "confined" -> sink += confinedBb(Confined.B, i);
                        default -> throw new IllegalArgumentException(row);
                    }
                }
                long ns = (System.nanoTime() - t0) / n;
                if (warm == 2) {
                    System.out.println(row + " " + ns + " ns/op");
                }
            }
            if (sink == 42) System.out.println("unlikely");
        }
    }
}
