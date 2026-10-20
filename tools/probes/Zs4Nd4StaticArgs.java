// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * zs s4/nd4: {@code main}'s {@code String[]} is built BEFORE the main class is
 * initialized (HotSpot's {@code java.c}: {@code CreateApplicationArgs} runs
 * before the JNI lookup that runs {@code <clinit>}).
 *
 * <p>A STATIC {@code main(String[])} whose {@code <clinit>} fills the heap to
 * the last 16-byte object (the {@code Zs4Nd4LauncherExact} fill). On HotSpot
 * the array already exists, so {@code main} is entered and its first
 * allocation fails there: the uncaught-exception dispatch reports it, and on
 * the full heap the handler itself fails, so stderr is exactly
 * {@code \nException: java.lang.OutOfMemoryError thrown from the
 * UncaughtExceptionHandler in thread "main"\n}, rc 1. A launcher that builds the
 * array AFTER {@code <clinit>} fails before {@code main} instead and prints
 * {@code ExceptionDescribe}'s 27 bytes {@code Exception in thread "main" }.
 * {@code main-ran} on stdout means the holder ran out before the heap did: not
 * a verdict. Usage: {@code java -Xmx16m Zs4Nd4StaticArgs}.
 */
public class Zs4Nd4StaticArgs {
    static Object[] chain;
    static Object[] smalls;

    static {
        System.out.println("start");
        smalls = new Object[1 << 16];
        int size = 1 << 20;
        Object[] head = null;
        try {
            while (true) {
                try {
                    Object[] node = new Object[2];
                    node[0] = size > 0 ? new long[size] : new Object();
                    node[1] = head;
                    head = node;
                } catch (OutOfMemoryError e) {
                    if (size == 0) {
                        break;
                    }
                    size >>= 1;
                }
            }
            Object[] holder = smalls;
            int n = 0;
            try {
                while (n < holder.length) {
                    holder[n] = new Object();
                    n++;
                }
            } catch (OutOfMemoryError e) {
                // The heap is full to the last 16 bytes.
            }
        } finally {
            chain = head;
        }
    }

    public static void main(String[] args) {
        System.out.println("main-ran " + args.length + " " + (chain != null));
    }
}
