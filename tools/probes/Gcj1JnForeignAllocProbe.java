// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j1/jn (2026-10-19): a foreign-attached thread's JNI allocations collect,
 * as HotSpot's do
 * ({@code docs/known-issues/gc/gcj-j1jn-foreign-attached-jni-allocation-never-collects-20261019.md}).
 *
 * <p>A host thread started by the native half
 * ({@code tools/probes/jni/Gcj1JnForeignAllocProbe.c}) attaches with
 * {@code AttachCurrentThread} and allocates {@code count} {@code byte[kib KiB]}
 * with {@code NewByteArray} while this Java thread only sleeps, so no Java
 * allocation ever reaches a collection door. Mode {@code drop} deletes each
 * array at once; mode {@code ring} keeps the last 8 as local refs and checks
 * their stamped bytes after every allocation. HotSpot ({@code -XX:+UseSerialGC
 * -Xmx64m}) collects inside the refused allocation and prints, for the
 * defaults ({@code drop 1024 256}, 256 MiB of garbage):
 *
 * <pre>
 *   foreign-alloc drop: PASS arrays=1024
 * </pre>
 *
 * and for {@code ring 1024 256}:
 *
 * <pre>
 *   foreign-alloc ring: PASS arrays=1024 bad=0
 * </pre>
 *
 * A refused allocation prints {@code FAIL oome-at=<i> of <count>} instead
 * (the attachment's {@code OutOfMemoryError} was cleared by the native).
 * stderr only: {@code [gcj1jn-fa] wall_ms=<n>}.
 *
 * <p>Usage: {@code Gcj1JnForeignAllocProbe <library> [drop|ring] [count] [kib] [native|poll]}.
 * {@code native} (the default) waits in {@code pthread_join} inside the native
 * {@code join}: with CratonVM's JNI package (default ON) that thread is in
 * native, so it neither holds the attachment's collections nor reaches a
 * collection door of its own -- the attachment is the only allocator and the
 * only possible requester, which is the case the probe exists for. {@code poll}
 * sleeps in Java and polls {@code finished()} (for a run with the package off,
 * {@code CRATONVM_JNI_NATIVE_TRANSITIONS=0}, where a thread blocked in C would
 * be a counted mutator and hold every pause); its Java-side boundaries may
 * collect on the attachment's behalf, so it is not the positive control.
 * Build lines: the C file's header comment.
 */
public final class Gcj1JnForeignAllocProbe {

    static native int start(int mode, int count, int kib);

    static native boolean finished();

    static native void join(int[] out);

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        String mode = args.length > 1 ? args[1] : "drop";
        int count = args.length > 2 ? Integer.parseInt(args[2]) : 1024;
        int kib = args.length > 3 ? Integer.parseInt(args[3]) : 256;
        boolean poll = args.length > 4 && args[4].equals("poll");
        int m = mode.equals("ring") ? 1 : 0;
        long t0 = System.nanoTime();
        int rc = start(m, count, kib);
        if (rc != 0) {
            System.out.println("foreign-alloc " + mode + ": FAIL start=" + rc);
            System.exit(1);
        }
        if (poll) {
            long deadline = System.nanoTime() + 240_000_000_000L;
            while (!finished()) {
                if (System.nanoTime() > deadline) {
                    System.out.println("foreign-alloc " + mode + ": FAIL timeout");
                    System.exit(1);
                }
                Thread.sleep(5);
            }
        }
        int[] r = new int[4];
        join(r);
        System.err.println("[gcj1jn-fa] wall_ms=" + (System.nanoTime() - t0) / 1_000_000);
        StringBuilder line = new StringBuilder("foreign-alloc ").append(mode).append(": ");
        boolean ok = r[0] == 0 && r[2] < 0 && r[1] == count && r[3] == 0;
        if (r[0] != 0) {
            line.append("FAIL attach");
        } else if (r[2] >= 0) {
            line.append("FAIL oome-at=").append(r[2]).append(" of ").append(count);
        } else {
            line.append(ok ? "PASS" : "FAIL").append(" arrays=").append(r[1]);
        }
        if (m == 1 && r[0] == 0 && r[2] < 0) {
            line.append(" bad=").append(r[3]);
        }
        System.out.println(line);
        System.exit(ok ? 0 : 1);
    }
}
