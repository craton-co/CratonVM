#!/bin/sh
# Interpreter round i5 waves 107-108, lane L5: JDK 25 `URLClassPath.JarLoader.
# parseClassPath` resolves each manifest `Class-Path` token with
# `new URL(base, token)`. A drive spelling such as `C:/nowhere/x.jar` is the
# URL scheme `c` (on every OS). Its handler lookup runs
# `URL.lookupViaProviders`, a `ServiceLoader` of `URLStreamHandlerProvider`
# over the SYSTEM loader, whose `getResources` walks the application
# `URLClassPath` again (re-entrant) and opens the jars still unopened. Over
# the application loader that opens good.jar, whose `http:` token needs a
# provider lookup too; the thread is already looking up (`ThreadTracker`), so
# HotSpot throws `Error("Circular loading of URL stream handler providers
# detected")`, which escapes into the `bad.B` lookup; bad.jar and good.jar
# were both polled and never added, so the application loader never sees
# them again (dep.jar was never pushed). Over a URLClassLoader of its own the
# lookup finishes (no provider), `MalformedURLException: unknown protocol: c`
# is swallowed by `URLClassPath.getLoader` and bad.jar is skipped whole, and
# good.jar's `http:` token is only ignored (`tryResolveFile`).
# Wave 107 modelled only the second case; wave 108 models JDK 25's lazy
# expansion (`classloading/src/class_path.rs` `simulate_url_class_path`) and,
# under `--jdk-only`, lets the JDK's own `URLClassPath` answer the application
# loader's resources too (not built or run).
#
# Builds out/L5W107CP/{main,bad,dep,good}.jar. Run from this directory with a
# JDK 25 javac, jar and java on PATH:
#
#   sh setup.sh
#   java -cp out/L5W107CP/main.jar L5W107ClassPathDriveToken
#   java -jar out/L5W107CP/main.jar
#   cratonvm --jdk-only   [--nojit] -cp out/L5W107CP/main.jar L5W107ClassPathDriveToken
#   cratonvm --compatible [--nojit] -cp out/L5W107CP/main.jar L5W107ClassPathDriveToken
#   cratonvm --jdk-only   [--nojit] -jar out/L5W107CP/main.jar
#   cratonvm --compatible [--nojit] -jar out/L5W107CP/main.jar
#
# HOTSPOT_EXPECTED: rows 1-3 MEASURED on HotSpot 25.0.3 for Windows (wave
# 107 orchestrator run, `-cp` and `-jar`); the other rows derived from the
# JDK 25 sources (the same on Linux: the token is the scheme `c` there too):
# HOTSPOT_EXPECTED_BEGIN
# app bad.B=java.lang.Error: Circular loading of URL stream handler providers detected
# app dep.D=java.lang.ClassNotFoundException: dep.D
# app good.G=java.lang.ClassNotFoundException: good.G
# app bad/b.txt=null
# app good/g.txt=null
# bad.jar bad.B=java.lang.ClassNotFoundException: bad.B
# bad.jar bad/b.txt=null
# good.jar good.G=found loader=url
# good.jar good/g.txt=from-good
# HOTSPOT_EXPECTED_END
set -e
OUT=out/L5W107CP
rm -rf "$OUT"
mkdir -p "$OUT/src/bad" "$OUT/src/dep" "$OUT/src/good" "$OUT/badcls" "$OUT/depcls" "$OUT/goodcls" "$OUT/main"
printf 'package bad;\npublic class B { }\n' > "$OUT/src/bad/B.java"
printf 'package dep;\npublic class D { }\n' > "$OUT/src/dep/D.java"
printf 'package good;\npublic class G { }\n' > "$OUT/src/good/G.java"
javac --release 8 -d "$OUT/badcls" "$OUT/src/bad/B.java"
javac --release 8 -d "$OUT/depcls" "$OUT/src/dep/D.java"
javac --release 8 -d "$OUT/goodcls" "$OUT/src/good/G.java"
printf 'from-bad' > "$OUT/badcls/bad/b.txt"
printf 'from-good' > "$OUT/goodcls/good/g.txt"
printf 'Class-Path: dep.jar C:/nowhere/x.jar\n' > "$OUT/bad.txt"
printf 'Class-Path: http://example.invalid/y.jar\n' > "$OUT/good.txt"
jar --create --file "$OUT/bad.jar" --manifest "$OUT/bad.txt" -C "$OUT/badcls" .
jar --create --file "$OUT/dep.jar" -C "$OUT/depcls" .
jar --create --file "$OUT/good.jar" --manifest "$OUT/good.txt" -C "$OUT/goodcls" .
javac --release 11 -d "$OUT/main" L5W107ClassPathDriveToken.java
printf 'Main-Class: L5W107ClassPathDriveToken\nClass-Path: bad.jar good.jar\n' > "$OUT/manifest.txt"
jar --create --file "$OUT/main.jar" --manifest "$OUT/manifest.txt" -C "$OUT/main" .
