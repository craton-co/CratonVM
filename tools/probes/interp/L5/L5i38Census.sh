#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
#
# `i38-L5` closure (2026-11-02): the host census the page has asked for since
# wave 51. Reads workload command lines on stdin, one per line, with the
# literal token @MODE@ where the mode flag goes, e.g.
#
#   B=target/release/cratonvm tools/probes/interp/L5/L5i38Census.sh <<'EOF'
#   $B @MODE@ -cp out L5W125GlobalNameRefusalRecord
#   $B @MODE@ -jar spring-boot-app.jar
#   EOF
#
# Each line runs in --real-jdk and --compatible, with
# CRATONVM_GLOBAL_NAME_REFUSAL_RECORD off and on, under CRATONVM_DBG=access, and
# prints one row per run:
#   <mode> <refusal-switch> initiating-memo-only=<n> initiating-memo-only-global=<n>
#   global-name-refusal-memo=<base-miss>/<degrade> define-denied=<n> exit=<code>
# plus the last INITIATING-RECORD DENY line if any (a define the refusal turned
# into a LinkageError: with the switch on, each must be a define HotSpot refuses
# too). The decision rules are in the page's "Closure (2026-11-02)".
set -u
while IFS= read -r line; do
  [ -z "$line" ] && continue
  for mode in --real-jdk --compatible; do
    for sw in 0 1; do
      cmd="${line//@MODE@/$mode}"
      err="$(mktemp)"
      if [ "$sw" = 1 ]; then
        env CRATONVM_GLOBAL_NAME_REFUSAL_RECORD=1 CRATONVM_DBG=access bash -c "$cmd" >/dev/null 2>"$err"
      else
        env CRATONVM_DBG=access bash -c "$cmd" >/dev/null 2>"$err"
      fi
      code=$?
      census="$(grep -E 'LOADER census' "$err" | tail -n 1)"
      field() { printf '%s\n' "$census" | grep -oE "$1=[0-9/]+" | head -n 1; }
      printf '%s refusal=%s %s %s %s %s exit=%s\n' "$mode" "$sw" \
        "$(field initiating-memo-only)" "$(field initiating-memo-only-global)" \
        "$(field global-name-refusal-memo)" "$(field initiating-define-denied)" "$code"
      grep -E 'INITIATING-RECORD DENY' "$err" | tail -n 1
      rm -f "$err"
    done
  done
done
