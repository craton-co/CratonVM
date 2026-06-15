#!/usr/bin/env bash
# Run given test classes under the WORKTREE cratonvm binary vs HotSpot and show
# parity. Usage: verify-fix.sh <module> <FQCN> [FQCN...]
#   module = spring-core, spring-beans, ... (for the classpath)
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
SPRING="/c/craton/cratonvm/apps/spring-framework"
H="/c/craton/CratonVM-spring/spring-suite"
VM="${VM:-/c/craton/CratonVM-spring/target/release/cratonvm.exe}"
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
HS="$JDK\\bin\\java.exe"
MOD="$1"; shift
CPF="$SPRING/$MOD/build/cratonvm-testcp.txt"
CP="$(cygpath -m "$H");$(tr -d '\r' < "$CPF")"
AF="$H/.af_verify.txt"; { echo "-cp"; echo "$CP"; } > "$AF"; AFM=$(cygpath -m "$AF")
for cls in "$@"; do
  cv=$(timeout 120 "$VM" --java-home "$JDK" --stack-dump-on-timeout 0 "@$AFM" KRun "$cls" 2>/dev/null | grep -m1 '^RESULT ' | sed -E 's/.*status=([A-Z]+).* found=([0-9]+).* fail=([0-9]+).*/\1 found=\2 fail=\3/; t; s/.*status=([A-Z]+).*/\1/')
  cvrc=$?; [ $cvrc -eq 124 ] && cv="TIMEOUT/HANG"
  hs=$(timeout 120 "$HS" "@$AFM" KRun "$cls" 2>/dev/null | grep -m1 '^RESULT ' | sed -E 's/.*found=([0-9]+).* fail=([0-9]+).* status=([A-Z]+).*/\3 found=\1 fail=\2/')
  printf '%-66s  CV[%s]   HS[%s]\n' "${cls##*.}" "${cv:-ABEND}" "${hs:-?}"
done
