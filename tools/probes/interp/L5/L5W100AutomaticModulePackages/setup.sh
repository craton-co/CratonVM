#!/bin/sh
# Interpreter round i5 wave 100, lane L5: build the module path of
# L5W100AutomaticModulePackages (see src/app/app/Main.java for the run commands
# and the expected HotSpot output). Run from this directory with JDK 25's javac
# and jar on PATH:
#
#   sh setup.sh [outdir]        (default out/L5W100AM; the module path is <outdir>/mp)
#
# <outdir>/mp/my-lib-2.5.1-rc1.jar  no module-info; Automatic-Module-Name: lib.auto;
#                                   classes in lib, lib.sub; resources only in
#                                   templates/ and conf/x/
# <outdir>/mp/other-1.0.jar         automatic module other (nobody requires it)
# <outdir>/mp/app.jar               module app, requires lib.auto
set -e
OUT=${1:-out/L5W100AM}
rm -rf "$OUT"
mkdir -p "$OUT/lib/templates" "$OUT/lib/conf/x" "$OUT/app" "$OUT/mp"

javac -d "$OUT/lib" src/lib/lib/A.java src/lib/lib/sub/B.java
printf 'page\n' > "$OUT/lib/templates/page.txt"
printf 'k=v\n' > "$OUT/lib/conf/x/settings.properties"
printf 'Automatic-Module-Name: lib.auto\n' > "$OUT/manifest.txt"
jar --create --file "$OUT/mp/my-lib-2.5.1-rc1.jar" --manifest "$OUT/manifest.txt" -C "$OUT/lib" .

mkdir -p "$OUT/other"
javac -d "$OUT/other" src/other/other/O.java
jar --create --file "$OUT/mp/other-1.0.jar" -C "$OUT/other" .

javac --module-path "$OUT/mp" -d "$OUT/app" src/app/module-info.java src/app/app/Main.java
jar --create --file "$OUT/mp/app.jar" -C "$OUT/app" .
