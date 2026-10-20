#!/bin/sh
# Interpreter round i4 wave 99, lane L5: build the module path of
# L5W99ModuleVersionClauses (see src/app/app/Main.java for the run commands and
# the expected HotSpot output). Run from this directory with JDK 25's javac and
# jar on PATH:
#
#   sh setup.sh [outdir]        (default out/L5W99MV; the module path is <outdir>/mp)
#
# <outdir>/mp/app.jar      module app, `jar --module-version 1.2`
# <outdir>/mp/lib-3.4.jar  no module-info: automatic module lib, version 3.4
# <outdir>/mp/unv.jar      module unv, no version
#
# app is compiled against a public lib.Hidden; the lib-3.4.jar it runs against
# ships the package-private one (src/lib-final), so `new lib.Hidden()` is an
# IllegalAccessError.
set -e
OUT=${1:-out/L5W99MV}
rm -rf "$OUT"
mkdir -p "$OUT/lib" "$OUT/lib-final" "$OUT/unv" "$OUT/app" "$OUT/mp"

javac -d "$OUT/lib" src/lib/lib/Thing.java src/lib/lib/Hidden.java
jar --create --file "$OUT/mp/lib-3.4.jar" -C "$OUT/lib" .

javac -d "$OUT/unv" src/unv/module-info.java src/unv/unv/U.java src/unv/unv/hidden/H.java
jar --create --file "$OUT/mp/unv.jar" -C "$OUT/unv" .

javac --module-path "$OUT/mp" -d "$OUT/app" \
    src/app/module-info.java src/app/app/Main.java src/app/app/Box.java src/app/app/Other.java
jar --create --file "$OUT/mp/app.jar" --module-version 1.2 -C "$OUT/app" .

javac -d "$OUT/lib-final" src/lib/lib/Thing.java src/lib-final/lib/Hidden.java
rm -f "$OUT/mp/lib-3.4.jar"
jar --create --file "$OUT/mp/lib-3.4.jar" -C "$OUT/lib-final" .
