// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
public class ColdThrowLocalsProbe {
    static final class Buf {
        int readerIndex;
        int writerIndex = 100000;
        void ensureAccessible() { if (writerIndex < 0) throw new IllegalStateException("released"); }
        void checkReadableBytes0(int minimum) {
            ensureAccessible();
            if (readerIndex > writerIndex - minimum) {
                throw new IndexOutOfBoundsException(String.format(
                    "readerIndex(%d) + length(%d) exceeds writerIndex(%d): %s",
                    readerIndex, minimum, writerIndex, this));
            }
        }
        int readByte() { checkReadableBytes0(1); return readerIndex++; }
        public String toString() { return "buf"; }
    }
    static final class Response {
        boolean committed;
        boolean isCommitted() { return committed; }
        void checkCommitted(String messageKey) {
            if (isCommitted()) {
                throw new IllegalStateException(getString(messageKey));
            }
        }
        String getString(String key) {
            if (key == null) throw new IllegalArgumentException("key may not have a null value");
            return key;
        }
        void resetBuffer() { checkCommitted("resetBuffer.ise"); }
    }
    public static void main(String[] args) {
        Buf buf = new Buf();
        for (int n = 0; n < 5000; n++) if (buf.readByte() != n) throw new AssertionError();
        buf.readerIndex = buf.writerIndex;
        try { buf.readByte(); throw new AssertionError("missing exception"); }
        catch (IndexOutOfBoundsException expected) {
            String message = "readerIndex(100000) + length(1) exceeds writerIndex(100000): buf";
            if (!message.equals(expected.getMessage())) throw new AssertionError(expected);
        }
        Response response = new Response();
        for (int n = 0; n < 5000; n++) response.resetBuffer();
        response.committed = true;
        try { response.resetBuffer(); throw new AssertionError("missing committed exception"); }
        catch (IllegalStateException expected) {
            if (!"resetBuffer.ise".equals(expected.getMessage())) throw new AssertionError(expected);
        }
        System.out.println("COLD-THROW-LOCALS-OK");
    }
}
