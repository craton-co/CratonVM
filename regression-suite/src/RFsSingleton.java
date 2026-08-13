// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// W8-C14: the default FileSystem and its FileSystemProvider are SINGLETONS,
// and every door onto them must hand back the SAME OBJECT.
//
//   docs/known-issues/jdk-only/W8-C14-1-default-filesystem-second-door.md
//   docs/known-issues/jdk-only/W8-C14-2-default-provider-singleton.md
//
// Discipline this file is written to:
//
//  * Every comparison is `==`, never `.equals`. This is a singleton contract,
//    and an equality-shaped assertion passes against EXACTLY the defect these
//    checks exist to catch: two distinct objects that compare equal and (via
//    `jdk_concrete_getclass_alias`) even report the same class name. That is
//    not a hypothetical -- the mutation control for this fixture
//    (scratchpad/c14/FsIdentityMutant.java) builds a second platform
//    filesystem by hand, and `path2.equals(path1)` stays GREEN right through
//    it while every `==` row flips.
//  * NO reflection and NO --add-opens. The `sun.nio.fs.
//    DefaultFileSystemProvider` doors need `--add-opens java.base/
//    sun.nio.fs=ALL-UNNAMED` on BOTH arms to be comparable, and the suite
//    runner does not pass it; those rows live in the scratchpad probe
//    instead. Everything below is reachable from ordinary exported API, so
//    this fixture needs no runner flag at all.
//  * These rows are NOT a ratchet. Before W8-C14 landed, `provider()` and
//    `installedProviders()` allocated a fresh provider on every call and
//    `FileSystemProvider.getFileSystem(URI)` allocated a fresh FileSystem, so
//    the four `prov.*` rows and `fs.viaUri` were measured RED. The two
//    `fs.default*` rows were already green (the `p57_default_filesystem_
//    singleton` stash predates this work) and are here as the anchor that
//    proves the file system half stayed fixed.
//
// Every expectation was measured on Microsoft OpenJDK 25.0.3+9 (HotSpot is
// the oracle), transcript in W8-C14-1 section 2.
//
// OUTPUT DIALECT -- read this before changing a println.
//
// run.sh's extract() keeps ONLY lines prefixed `PASS ` or `CK ` and deletes
// everything else before the cross-VM diff. This file used to print
// `ok <name>` per check and end on `@@RESULT checks=12 fails=0`: 13 lines out
// of 13 were DELETED, the cross-VM key was the empty string, and no
// `PASS RFsSingleton` line was ever printed -- so run.sh scored the vector
// `FAIL no PASS line` on any VM, including a correct one, and guards G4+G2+G3
// all fired on the ORACLE arm. Measured on HotSpot 25.0.3+9; the fourth
// instance of the shape repaired in
// docs/known-issues/jdk-only/W8-E9-1-three-broken-oracles-and-the-suite-denominator.md.
//
// So: every check funnels through check()/checkStr(), which print
// `CK RFsSingleton <name>=<ACTUAL>`. The value is the VM's own answer, never
// this file's expectation, so the two VMs diff their answers against EACH
// OTHER and not each against a constant. The tail is
// `CK RFsSingleton fails=N`, then `CK RFsSingleton checks=N`, then
// `PASS RFsSingleton (N checks)` on the clean path only.
//
// `fails` and `checks` are on SEPARATE lines deliberately.
// harness_check_count does `sub(/^.*checks=/, ""); print`, i.e. it takes the
// whole rest of the line, so a combined `CK ... checks=12 fails=0` yields the
// "count" `12 fails=0` and G3's numeric test then errors out inside its own
// `2>/dev/null`. One value per line.
//
// `@@RESULT` was DROPPED rather than kept alongside: it is on a deleted
// prefix, so keeping it would re-fire G1 for one line. Checked before
// removing -- nothing under regression-suite/ parses `@@RESULT`, and the
// runners that do (apps/hib-suite-runner, probes/) never run this class.

import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.spi.FileSystemProvider;
import java.util.List;

public class RFsSingleton {

    static int checks = 0;
    static int fails = 0;

    // The single funnel. It prints the ACTUAL answer unconditionally -- a line
    // that only appears when a check passes is a line the diff cannot use, and
    // one that carries the EXPECTED value would make both VMs print the same
    // text whatever they did.
    static void check(String name, boolean actual, boolean expected) {
        checks++;
        System.out.println("CK RFsSingleton " + name + "=" + actual);
        if (actual != expected) {
            fails++;
            System.out.println("CK RFsSingleton FAILED " + name + " expected=" + expected);
        }
    }

    // Same funnel for the one check whose observable is a string rather than a
    // boolean. It must not collapse to `scheme.equals("file")`: the defect this
    // vector was written against answered "jrt" for the platform file system,
    // and a boolean would tell the reader only that something was wrong.
    static void checkStr(String name, String actual, String expected) {
        checks++;
        System.out.println("CK RFsSingleton " + name + "=" + actual);
        if (!expected.equals(actual)) {
            fails++;
            System.out.println("CK RFsSingleton FAILED " + name + " expected=" + expected);
        }
    }

    // ---- the default FileSystem is one object -------------------------------

    static void defaultFileSystem() {
        FileSystem a = FileSystems.getDefault();
        FileSystem b = FileSystems.getDefault();
        check("fs.defaultStable", a == b, true);

        // Paths must be minted BY that filesystem, not by a second one.
        check("fs.defaultPathsGet", Paths.get("x").getFileSystem() == a, true);
        check("fs.defaultPathOf", Path.of("x").getFileSystem() == a, true);
        check("fs.defaultGetPath", a.getPath("x").getFileSystem() == a, true);

        // FileSystems.getFileSystem(file:///) resolves the "file" provider and
        // calls FileSystemProvider.getFileSystem(URI) on it. That returned a
        // freshly allocated FileSystem before W8-C14.
        FileSystem viaUri = FileSystems.getFileSystem(URI.create("file:///"));
        check("fs.viaUri", viaUri == a, true);
    }

    // ---- the default provider is one object ---------------------------------

    static void defaultProvider() {
        FileSystem fs = FileSystems.getDefault();

        // Self-identity. This was false before W8-C14: `provider()` allocated a
        // new object per call, so a FileSystem did not even agree with itself.
        FileSystemProvider p1 = fs.provider();
        FileSystemProvider p2 = fs.provider();
        check("prov.stable", p1 == p2, true);

        // The scheme must survive the caching, and must be "file" for the
        // platform default -- not "jrt". (The per-call code this replaced
        // sniffed slots 1/2 for jar/jrt markers, which on a REAL
        // sun.nio.fs.WindowsFileSystem receiver hold defaultDirectory and
        // defaultRoot: both non-null, so it answered "jrt" for the platform's
        // own file system.)
        checkStr("prov.scheme", p1.getScheme(), "file");

        // The javadoc of FileSystemProvider.installedProviders makes the default
        // provider the FIRST element of that list.
        List<FileSystemProvider> ip1 = FileSystemProvider.installedProviders();
        List<FileSystemProvider> ip2 = FileSystemProvider.installedProviders();
        check("prov.installedIsDefault", ip1.get(0) == p1, true);
        check("prov.installedStable", ip1.get(0) == ip2.get(0), true);

        // A path's filesystem's provider is the same provider.
        check("prov.viaPath", Paths.get("x").getFileSystem().provider() == p1, true);
    }

    // ---- negative controls --------------------------------------------------
    //
    // If EVERY row in this fixture is expected true, a `==` that had degenerated
    // into something always-true would pass the whole file. These two rows must
    // be FALSE on HotSpot 25 (measured), so they fail loudly if identity
    // comparison stops discriminating.

    static void negativeControls() {
        FileSystem fs = FileSystems.getDefault();
        // Path identity is NOT contracted anywhere in the JDK; two getPath
        // calls return distinct objects on HotSpot.
        check("neg.pathsDistinct", fs.getPath("x") == fs.getPath("x"), false);
        check("neg.objectsDistinct", new Object() == new Object(), false);
    }

    public static void main(String[] args) {
        defaultFileSystem();
        defaultProvider();
        negativeControls();
        System.out.println("CK RFsSingleton fails=" + fails);
        System.out.println("CK RFsSingleton checks=" + checks);
        if (fails != 0) {
            throw new RuntimeException(fails + " default-filesystem singleton checks failed");
        }
        System.out.println("PASS RFsSingleton (" + checks + " checks)");
    }
}
