// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 53, lane L5 -- the positive control of the
// `jdk-file-bodies` HotSpot-alignment row: `--compatible` over a real JDK runs
// `java.io.File`'s own bodies (and the platform `FileSystem` wrappers'), as
// `--jdk-only` does, instead of the stand-ins that read the path out of the
// object's first slot (`docs/internal/proposals/interpreter/i52-L5-proposal-a-compatible-row-per-retired-shadow-family-20261016.md`).
// Rows, first (a cold VM) and again last:
//
//   topath   `File.toPath()` caches its `Path` (`filePath`): two calls answer
//            the same object. The stand-in minted a new `Path` per call.
//   sub      a `File` subclass that overrides `getPath()`: the JDK's
//            `toString()` and `compareTo` go through `getPath()`, its
//            `getName()` reads the field. The stand-ins read the field for all.
//   plain    host-independent answers both sides already agree on (a guard):
//            `getName`, `getParent` (separators printed as `/`), `isAbsolute`
//            of a relative path, `exists` of a fresh temp file, `length`.
//
// Run (no setup):
//   javac -d out L5W53CompatFileBodies.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W53CompatFileBodies
//
// Positive control (`--compatible`): `CRATONVM_COMPAT_ALIGN=-jdk-file-bodies`
// keeps the stand-ins (expected there: `topath-N=same=false` and
// `sub-N=toString=x compare=...` differing from HotSpot's).
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   topath-1=same=true
//   sub-1=toString=over name=x compare=true
//   plain-1=name=c.txt parent=a/b abs=false exists=true length=3
//   topath-2=same=true
//   sub-2=toString=over name=x compare=true
//   plain-2=name=c.txt parent=a/b abs=false exists=true length=3

import java.io.File;
import java.nio.file.Files;
import java.nio.file.Path;

public class L5W53CompatFileBodies {
    static final class Over extends File {
        Over(String p) {
            super(p);
        }

        @Override
        public String getPath() {
            return "over";
        }
    }

    static void round(int r) throws Exception {
        File f = new File("l5w53-topath");
        System.out.println("topath-" + r + "=same=" + (f.toPath() == f.toPath()));
        File o = new Over("x");
        System.out.println("sub-" + r + "=toString=" + o + " name=" + o.getName()
                + " compare=" + (o.compareTo(new File("over")) == 0));
        Path dir = Files.createTempDirectory("l5w53f");
        Path c = dir.resolve("c.txt");
        Files.writeString(c, "abc");
        File cf = c.toFile();
        File rel = new File("a/b/c.txt");
        System.out.println("plain-" + r + "=name=" + rel.getName()
                + " parent=" + rel.getParent().replace(File.separatorChar, '/')
                + " abs=" + rel.isAbsolute() + " exists=" + cf.exists() + " length=" + cf.length());
        Files.delete(c);
        Files.delete(dir);
    }

    public static void main(String[] args) throws Exception {
        for (int r = 1; r <= 2; r++) {
            round(r);
        }
    }
}
