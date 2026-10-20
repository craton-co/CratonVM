#!/bin/bash
# usage: sbr5-clload-chain-setup.sh <workdir> <abs jar> ... ; builds <workdir>/chain/booter.jar, a manifest-only jar whose Class-Path holds
# file:/ URLs of the first half of the jars, a missing entry, a duplicate and a nested manifest-only inner.jar (second half, pointing
# back at booter.jar: a cycle). JAVA_HOME = a JDK 25. Prints the booter path.
set -e
W=$1; shift; mkdir -p $W; cd $W; J=$JAVA_HOME/bin; rm -rf chain; mkdir -p chain/e; cd chain
jars=("$@"); n=${#jars[@]}; half=$((n/2))
fold_mf() { printf 'Manifest-Version: 1.0\n'; printf '%s' "Class-Path: $1" | fold -w 70 | sed '2,$ s/^/ /'; printf '\n'; }
inner="booter.jar"; for ((i=half;i<n;i++)); do inner="$inner file:${jars[$i]}"; done
fold_mf "$inner" > inner.mf; $J/jar --create --file inner.jar --manifest inner.mf -C e .
outer=""; for ((i=0;i<half;i++)); do outer="$outer file:${jars[$i]}"; done
outer="$outer missing-nothing.jar file:${jars[0]} inner.jar"
fold_mf "$outer" > booter.mf; $J/jar --create --file booter.jar --manifest booter.mf -C e .
echo "$PWD/booter.jar"
