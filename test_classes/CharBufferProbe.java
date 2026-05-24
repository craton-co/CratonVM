// SPDX-License-Identifier: Apache-2.0
// Probe for the "CharBuffer has no backing array" bug surfaced by
// icu4j-68.2 / icu4j-70.1: ByteBuffer.asCharBuffer() must produce a
// CharBuffer whose charAt(i) and subSequence(i,j).toString() return
// the correct UTF-16 code units. The legacy synthetic CharBuffer
// view created by `s2_bb_as_char_buffer` carried the underlying
// byte[] in slot 0 (BB_ARRAY), but the CharBuffer native dispatch
// path resolved slot 0 as `mark` when the real-JDK CharBuffer/Buffer
// class file was loaded — yielding Int(-1) and the "no backing array"
// IllegalStateException.
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.StandardCharsets;

public class CharBufferProbe {
    public static void main(String[] args) throws Exception {
        // (1) Heap CharBuffer.wrap(char[]) — must hasArray() == true.
        char[] chars = { 'H', 'e', 'l', 'l', 'o' };
        CharBuffer cb1 = CharBuffer.wrap(chars);
        if (!cb1.hasArray()) {
            throw new AssertionError("CharBuffer.wrap(char[]).hasArray() == false");
        }
        char[] arr = cb1.array();
        if (arr == null || arr.length < chars.length) {
            throw new AssertionError("CharBuffer.wrap(char[]).array() lost data");
        }
        System.out.println("ok wrap(char[]): hasArray=true, array.length=" + arr.length);

        // (2) ByteBuffer.asCharBuffer() — view buffer, charAt(i) and
        //     subSequence(i,j).toString() must succeed.
        byte[] raw = "Hi!\0".getBytes(StandardCharsets.UTF_16BE);
        ByteBuffer bb = ByteBuffer.wrap(raw);
        CharBuffer cb2 = bb.asCharBuffer();
        char c0 = cb2.charAt(0);
        if (c0 != 'H') {
            throw new AssertionError("asCharBuffer().charAt(0) != 'H', got " + (int) c0);
        }
        CharSequence sub = cb2.subSequence(0, 2);
        String s = sub.toString();
        if (!"Hi".equals(s)) {
            throw new AssertionError("asCharBuffer().subSequence(0,2).toString() != \"Hi\", got " + s);
        }
        System.out.println("ok asCharBuffer(): charAt(0)='H', subSequence=\"Hi\"");

        // (3) CharBuffer.wrap(CharSequence) — read-only, hasArray() must
        //     match HotSpot (false in real-JDK; whatever we return must
        //     not throw "no backing array" when array() is gated on it).
        CharBuffer cb3 = CharBuffer.wrap("Probe");
        if (cb3.hasArray()) {
            // OK, but then array() must not throw.
            char[] a = cb3.array();
            if (a == null) {
                throw new AssertionError("CharBuffer.wrap(String).array() returned null after hasArray=true");
            }
        } else {
            // Match HotSpot.
            try {
                cb3.array();
                throw new AssertionError("CharBuffer.wrap(String).array() should throw UnsupportedOperationException");
            } catch (UnsupportedOperationException ok) {
                // expected
            }
        }
        System.out.println("ok wrap(CharSequence): hasArray=" + cb3.hasArray());

        System.out.println("CharBufferProbe PASS");
    }
}
