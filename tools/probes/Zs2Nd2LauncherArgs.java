// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * zs s2/nd2: the launcher's {@code String[]} for {@code main} on a heap the
 * main class's initializer filled (host row {@code nd_L1s_co}, rc 134:
 * {@code FATAL: ZGC(real): array allocation of 16 bytes cannot be served}).
 *
 * <p>CratonVM's launcher builds the instance receiver and THEN the argument
 * array, both after {@code <clinit>}. {@code Zs1NdLauncherOome} hit the abort
 * only when its object-granularity fill happened to leave room for the
 * receiver and not for the array. This probe makes that the common case: the
 * fill keeps the heap full, then drops exactly ONE small object at the end of
 * the initializer, so the receiver's collection frees room for the receiver
 * and (about) nothing more.
 *
 * <p>HotSpot builds {@code String[]} before the class is initialized and
 * reports a failure before {@code main} through {@code ExceptionDescribe}:
 * stdout {@code start}, stderr exactly {@code Exception in thread "main" }
 * (no newline) when even the report cannot allocate, or {@code main-ran
 * true} when the room sufficed. Either is a pass for CratonVM as long as the
 * process does not abort (rc 134) and stdout/stderr match HotSpot's pair.
 * Usage: {@code java -Xmx16m Zs2Nd2LauncherArgs}.
 */
public class Zs2Nd2LauncherArgs {
    static Object[] chain;
    static Object spare;

    static {
        System.out.println("start");
        spare = new Object();
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
        } finally {
            chain = head;
            spare = null;
        }
    }

    void main() {
        System.out.println("main-ran " + (chain != null));
    }
}
