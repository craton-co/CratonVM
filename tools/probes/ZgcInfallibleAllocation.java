// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.*;
import java.lang.invoke.*;
import java.util.Locale;

/** First-use native allocations at a genuinely full heap, followed by recovery.
 * Each operation must either return the correct value or throw a Java OOME;
 * every other throwable fails the process. FFM uses strcat, whose non-null
 * ADDRESS return requires a new Java segment after the C call.
 */
public class ZgcInfallibleAllocation {
    static String heapMessage = "Java heap space";
    static String overheadMessage = "GC overhead limit exceeded";
    static byte[][] keep;
    static int occupied;
    static String source;
    static MethodHandle strcat;
    static MemorySegment text;
    static MemorySegment suffix;
    static Object target;
    static class MirrorTarget {}

    static String lower(String s) { return s.toLowerCase(Locale.ROOT); }
    static Class<?> klass(Object value) { return value.getClass(); }
    static MemorySegment address() throws Throwable {
        return (MemorySegment) strcat.invokeExact(text, suffix);
    }
    static void resetText() {
        text.set(ValueLayout.JAVA_BYTE, 0, (byte) 'a');
        text.set(ValueLayout.JAVA_BYTE, 1, (byte) 0);
    }
    static void checkNativeEffect(boolean completed) {
        int length = 0;
        while (length < 8 && text.get(ValueLayout.JAVA_BYTE, length) != 0) length++;
        if (length > 2 || length < 1 || (completed && length != 2))
            throw new AssertionError("native call repeated or lost");
    }
    static void fill() {
        keep = new byte[131072][];
        occupied = 0;
        try { while (occupied < keep.length) keep[occupied++] = new byte[8192]; }
        catch (OutOfMemoryError expected) { occupied--; }
        try { while (occupied < keep.length) keep[occupied++] = new byte[1024]; }
        catch (OutOfMemoryError expected) { occupied--; }
        try { while (occupied < keep.length) keep[occupied++] = new byte[128]; }
        catch (OutOfMemoryError expected) { occupied--; }
        try { while (occupied < keep.length) keep[occupied++] = new byte[16]; }
        catch (OutOfMemoryError expected) { occupied--; }
        if (occupied == keep.length) throw new AssertionError("heap was not filled");
    }
    static void action(String mode) throws Throwable {
        switch (mode) {
            case "lower":
                String answer = lower(source);
                if (answer.length() != source.length() || answer.charAt(0) != 'a')
                    throw new AssertionError("bad lowercase result");
                break;
            case "mirror":
                if (klass(target) != MirrorTarget.class) throw new AssertionError("bad mirror");
                break;
            case "arraymirror":
                if (!klass(target).isArray()) throw new AssertionError("bad array mirror");
                break;
            case "ffm":
                resetText();
                if (address().address() != text.address()) throw new AssertionError("bad ADDRESS return");
                checkNativeEffect(true);
                break;
            default: throw new AssertionError(mode);
        }
    }
    public static void main(String[] args) throws Throwable {
        String mode = args[0];
        int warmup = args.length > 1 ? Integer.parseInt(args[1]) : 300000;
        if (mode.equals("lower")) {
            char[] chars = new char[1024]; java.util.Arrays.fill(chars, 'A');
            source = new String(chars);
            for (int i = 0; i < warmup; i++) lower("WARM");
        } else if (mode.equals("mirror")) {
            Object warm = new Object();
            for (int i = 0; i < warmup; i++) klass(warm);
            target = new MirrorTarget();
        } else if (mode.equals("arraymirror")) {
            Object warm = new int[0];
            for (int i = 0; i < warmup; i++) klass(warm);
            target = new MirrorTarget[1][1];
        } else if (mode.equals("ffm")) {
            Linker linker = Linker.nativeLinker();
            SymbolLookup lookup = SymbolLookup.libraryLookup(
                System.getProperty("os.name").startsWith("Windows") ? "msvcrt" : "libc.so.6", Arena.global());
            strcat = linker.downcallHandle(lookup.find("strcat").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
            text = Arena.global().allocate(64);
            suffix = Arena.global().allocateFrom("b");
            for (int i = 0; i < warmup; i++) { resetText(); address(); }
        }
        for (int r = 0; r < 3; r++) {
            fill();
            try { action(mode); }
            catch (OutOfMemoryError expected) {
                String message = expected.getMessage();
                if (message != null && !message.equals(heapMessage)
                    && !message.equals(overheadMessage)) throw expected;
            } finally { keep = null; }
            System.gc();
            if (mode.equals("ffm")) checkNativeEffect(false);
            action(mode);
        }
        System.out.println(mode + ": recovered");
    }
}
