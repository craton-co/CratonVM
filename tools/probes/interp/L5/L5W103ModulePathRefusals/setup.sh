#!/bin/sh
# Interpreter round i5 wave 103, lane L5
# (docs/internal/fixed-bugs/interpreter-i98-orch-module-clauses-omit-version-of-named-application-modules-FIXED-20261016.md,
# residue). Builds six module paths (and a non-JAR file) under out/L5W103MP and the class-path main
# class. Run from this directory with JDK 25's javac and java on PATH:
#
#   sh setup.sh
#   M=out/L5W103MP; C="-cp $M/main L5W103ModulePathRefusals"
#   java -p $M/dup $C                         cratonvm --java-home <jdk25> [--compatible] -p $M/dup $C
#   java -p $M/badname $C                     (same for every row)
#   java -p $M/badauto $C
#   java -p $M/toplevel $C
#   java -p $M/mr --add-modules mr $C
#   java -p $M/svc --add-modules impl $C
#   java -p $M/svc -m impl
#   java -p $M/notes.txt $C
#
# Expected (HotSpot 25, derived from jdk.internal.module.ModulePath; the
# error rows print on stdout and exit 1; on Windows the paths use `\`):
#   dup:      Error occurred during initialization of boot layer
#             java.lang.module.FindException: Two versions of module lib found in out/L5W103MP/dup (lib-1.0.jar and lib-2.0.jar)
#             (the two file names come in the directory's listing order,
#             second-listed first; the order above is the Linux w103 HotSpot
#             run's. CratonVM follows the same OS listing since the wave-103
#             hand-back)
#   badname:  Error occurred during initialization of boot layer
#             java.lang.module.FindException: Unable to derive module descriptor for out/L5W103MP/badname/1x.jar
#             Caused by: java.lang.IllegalArgumentException: 1x: Invalid module name: '1x' is not a Java identifier
#   badauto:  Error occurred during initialization of boot layer
#             java.lang.module.FindException: Unable to derive module descriptor for out/L5W103MP/badauto/named.jar
#             Caused by: java.lang.module.FindException: Automatic-Module-Name: a.b-c: Invalid module name: 'b-c' is not a Java identifier
#   toplevel: Error occurred during initialization of boot layer
#             java.lang.module.FindException: Unable to derive module descriptor for out/L5W103MP/toplevel/top.jar
#             Caused by: java.lang.module.InvalidModuleDescriptorException: Top.class found in top-level directory (unnamed package not allowed in module)
#   mr:       mr packages=[p, q] automatic=true
#             ver rawVersion=none
#             (q exists only under META-INF/versions/11/; r only under 99;
#             ver-1.0-.jar is module ver: `Version.parse("1.0-")` refuses the
#             tail, so it has no version)
#   svc:      ran
#             impl provides=[java.lang.Runnable with [impl.Hello]] mainClass=impl.Hello
#             runnable providers=[impl.Hello@impl]
#             (automatic module impl: META-INF/services is its `provides`,
#             Main-Class its main class)
#   svc -m:   hello from impl
#   notes:    Error occurred during initialization of boot layer
#             java.lang.module.FindException: Module format not recognized: out/L5W103MP/notes.txt
# Before wave 103 CratonVM printed `ran` on the four refusal rows that name a
# JAR or a directory (it left the jar out, or kept the first module),
# `mr packages=[p]`, `ver rawVersion=1.0-`, `impl provides=[] mainClass=none`
# (and no `impl.Hello@impl` provider), and refused `-m impl` for want of a
# ModuleMainClass.
set -e
OUT=out/L5W103MP
rm -rf "$OUT"
mkdir -p "$OUT/main" "$OUT/cls/l" "$OUT/cls/p" "$OUT/cls/q" "$OUT/cls/r" "$OUT/cls/top" "$OUT/tool"
javac --release 17 -d "$OUT/main" L5W103ModulePathRefusals.java
for pkg in l p q r; do
  mkdir -p "$OUT/src/$pkg"
  printf 'package %s;\npublic class A {}\n' "$pkg" > "$OUT/src/$pkg/A.java"
  javac --release 17 -d "$OUT/cls/$pkg" "$OUT/src/$pkg/A.java"
done
printf 'public class Top {}\n' > "$OUT/src/Top.java"
javac --release 17 -d "$OUT/cls/top" "$OUT/src/Top.java"
cat > "$OUT/tool/MakeJar.java" <<'JAVA'
import java.io.*;
import java.nio.file.*;
import java.util.jar.*;

public class MakeJar {
    // args: <jar> <manifest attrs "K: V;K: V" or -> then pairs <entry name> <file>
    public static void main(String[] args) throws Exception {
        Manifest mf = new Manifest();
        mf.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        if (!args[1].equals("-")) {
            for (String kv : args[1].split(";")) {
                String[] p = kv.split(": ", 2);
                mf.getMainAttributes().put(new Attributes.Name(p[0]), p[1]);
            }
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(args[0]), mf)) {
            for (int i = 2; i + 1 < args.length; i += 2) {
                out.putNextEntry(new JarEntry(args[i]));
                out.write(Files.readAllBytes(Paths.get(args[i + 1])));
                out.closeEntry();
            }
        }
    }
}
JAVA
J="java $OUT/tool/MakeJar.java"
mkdir -p "$OUT/dup" "$OUT/badname" "$OUT/badauto" "$OUT/toplevel" "$OUT/mr"
$J "$OUT/dup/lib-1.0.jar" - l/A.class "$OUT/cls/l/l/A.class"
$J "$OUT/dup/lib-2.0.jar" - l/A.class "$OUT/cls/l/l/A.class"
$J "$OUT/badname/1x.jar" - p/A.class "$OUT/cls/p/p/A.class"
$J "$OUT/badauto/named.jar" "Automatic-Module-Name: a.b-c" p/A.class "$OUT/cls/p/p/A.class"
$J "$OUT/toplevel/top.jar" - Top.class "$OUT/cls/top/Top.class" p/A.class "$OUT/cls/p/p/A.class"
$J "$OUT/mr/mr-1.0.jar" "Multi-Release: true" p/A.class "$OUT/cls/p/p/A.class" \
  META-INF/versions/11/q/A.class "$OUT/cls/q/q/A.class" \
  META-INF/versions/99/r/A.class "$OUT/cls/r/r/A.class"
$J "$OUT/mr/ver-1.0-.jar" - l/A.class "$OUT/cls/l/l/A.class"
mkdir -p "$OUT/cls/impl" "$OUT/svc"
javac --release 17 -d "$OUT/cls/impl" src/impl/impl/Hello.java
$J "$OUT/svc/impl-1.0.jar" "Main-Class: impl.Hello" impl/Hello.class "$OUT/cls/impl/impl/Hello.class" \
  META-INF/services/java.lang.Runnable src/impl/META-INF/services/java.lang.Runnable
printf 'not a module\n' > "$OUT/notes.txt"
echo "built $OUT"
