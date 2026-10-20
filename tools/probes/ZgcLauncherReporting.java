// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/** Controls for JNI launcher failures versus a failure inside main. */
public class ZgcLauncherReporting {
    /** Select Unicode that this HotSpot launcher's native charset can carry.
     * Emit ASCII UTF-16 hex so the controller need not guess charset aliases.
     */
    public static void main(String[] args) {
        String encoding = System.getProperty("native.encoding", "UTF-8");
        java.nio.charset.CharsetEncoder encoder = java.nio.charset.Charset.forName(encoding).newEncoder();
        for (String text : new String[] {"ñ🦀", "Ж", "é", "あ", "中", "£", "u"}) {
            if (!encoder.canEncode(text)) continue;
            StringBuilder hex = new StringBuilder();
            for (char ch : text.toCharArray())
                hex.append(String.format(java.util.Locale.ROOT, "%04x", (int) ch));
            System.out.println(encoding);
            System.out.println(hex);
            return;
        }
        throw new AssertionError("no representable argument");
    }
    public static class ConstructorFailure {
        static { Thread.setDefaultUncaughtExceptionHandler((t, e) -> System.out.println("UNEXPECTED_HANDLER")); }
        ConstructorFailure() { throw new IllegalStateException("constructor"); }
        void main() { throw new AssertionError("main entered"); }
    }
    public static class MainFailure {
        static { Thread.setDefaultUncaughtExceptionHandler((t, e) -> System.out.println(e.getClass().getName() + ":" + e.getMessage())); }
        public static void main(String[] args) { throw new IllegalStateException("inside main"); }
    }
    public static class Arguments {
        static { System.gc(); }
        Arguments() { System.gc(); }
        void main(String[] args) {
            if (args.length != 4 || !args[0].equals("one") || !args[1].equals("one")
                    || args[3].length() != 4 * args[2].length()) throw new AssertionError("bad arguments");
            for (int i = 0; i < args[2].length(); i++)
                if (args[2].charAt(i) != Integer.parseInt(args[3].substring(4 * i, 4 * i + 4), 16))
                    throw new AssertionError("argument text changed");
            if (args[0] == args[1] || args[0] == "one" || args[1] == "one")
                throw new AssertionError("launcher arguments were interned");
            System.out.println("arguments: fresh and preserved");
        }
    }
    public static class MainFailureAtFullHeap {
        static byte[][] keep;
        static volatile byte[] failureAllocation;
        static {
            Thread.setDefaultUncaughtExceptionHandler((t, e) -> failureAllocation = new byte[128 * 1024 * 1024]);
            keep = new byte[262144][];
            int n = 0;
            try { while (true) { keep[n] = new byte[8192]; n++; } }
            catch (OutOfMemoryError expected) { }
            try { while (true) { keep[n] = new byte[0]; n++; } }
            catch (OutOfMemoryError expected) { }
        }
        public static void main(String[] args) { throw new AssertionError("main failure under full heap"); }
    }
}
