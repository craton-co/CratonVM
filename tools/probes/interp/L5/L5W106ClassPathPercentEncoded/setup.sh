#!/bin/sh
# Interpreter round i5 wave 106, lane L5: a manifest `Class-Path` token is a
# URL relative to the jar (JDK 25 `URLClassPath.JarLoader.parseClassPath` ->
# `new URL(base, path)`), and the jar it names is opened through
# `ParseUtil.decode` of that URL's path. So `Class-Path: lib%20dir/dep.jar`
# names the file `lib dir/dep.jar`. CratonVM joined the token verbatim
# (`classloading/src/class_path.rs` `decode_manifest_classpath_entry`), so its
# own class path looked for a directory literally called `lib%20dir`.
#
# Builds out/L5W106CP/main.jar (Main-Class L5W106ClassPathPercentEncoded,
# Class-Path lib%20dir/dep.jar) and out/L5W106CP/lib dir/dep.jar (dep.D and
# dep/d.txt). Run from this directory with a JDK 25 javac, jar and java on PATH:
#
#   sh setup.sh
#   java -cp out/L5W106CP/main.jar L5W106ClassPathPercentEncoded
#   java -jar out/L5W106CP/main.jar
#   cratonvm --jdk-only   [--nojit] -cp out/L5W106CP/main.jar L5W106ClassPathPercentEncoded
#   cratonvm --compatible [--nojit] -cp out/L5W106CP/main.jar L5W106ClassPathPercentEncoded
#   cratonvm --jdk-only   [--nojit] -jar out/L5W106CP/main.jar
#   cratonvm --compatible [--nojit] -jar out/L5W106CP/main.jar
#
# HOTSPOT_EXPECTED derived from the JDK 25 sources, not run (every command):
# HOTSPOT_EXPECTED_BEGIN
# class=dep.D says=dep loader=app
# resource=from-dep
# HOTSPOT_EXPECTED_END
set -e
OUT=out/L5W106CP
rm -rf "$OUT"
mkdir -p "$OUT/src/dep" "$OUT/depcls" "$OUT/main" "$OUT/lib dir"
printf 'package dep;\npublic class D { public static String says() { return "dep"; } }\n' > "$OUT/src/dep/D.java"
javac --release 8 -d "$OUT/depcls" "$OUT/src/dep/D.java"
printf 'from-dep' > "$OUT/depcls/dep/d.txt"
jar --create --file "$OUT/lib dir/dep.jar" -C "$OUT/depcls" .
javac --release 11 -d "$OUT/main" L5W106ClassPathPercentEncoded.java
printf 'Main-Class: L5W106ClassPathPercentEncoded\nClass-Path: lib%%20dir/dep.jar\n' > "$OUT/manifest.txt"
jar --create --file "$OUT/main.jar" --manifest "$OUT/manifest.txt" -C "$OUT/main" .
