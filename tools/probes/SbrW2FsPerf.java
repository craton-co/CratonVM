// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.BasicFileAttributes;

/**
 * SBR wave 2, lane {@code fs}: a small timing micro over the retired
 * {@code UnixPath} / {@code UnixFileAttributes} surface (path algebra, and a
 * stat + attribute read), for an interleaved base-vs-lane A/B. Prints one
 * {@code ms} line per round and a checksum so both arms are known to do the
 * same work.
 */
public class SbrW2FsPerf {
    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        Path base = Path.of("/usr/lib/../lib/./x");
        Path tmp = Path.of(System.getProperty("java.io.tmpdir"));
        long sum = 0;
        for (int round = 0; round < 5; round++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                Path p = base.resolve("a" + (i & 7)).normalize();
                sum += p.getNameCount() + p.getFileName().toString().length() + p.hashCode()
                        + p.getParent().toString().length();
                if (p.startsWith(base.getRoot())) sum++;
            }
            long t1 = System.nanoTime();
            for (int i = 0; i < n / 20; i++) {
                BasicFileAttributes a = Files.readAttributes(tmp, BasicFileAttributes.class);
                sum += a.isDirectory() ? 1 : 0;
                sum += a.lastModifiedTime().toMillis() & 1;
            }
            long t2 = System.nanoTime();
            System.out.println("round " + round + " pathMs " + (t1 - t0) / 1_000_000
                    + " statMs " + (t2 - t1) / 1_000_000);
        }
        System.out.println("sum " + (sum != 0));
    }
}
