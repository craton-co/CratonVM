#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Report frame slots a JIT-compiled body READS and never WRITES.
#
#   CRATONVM_DBG_JIT_DISASM=<Class>.<method> cratonvm ... 2> dump.err
#   python tools/jit/frame-slot-scan.py dump.err
#
# A body that reads `[rbp-XXh]` and never writes it is reading uninitialised
# stack. That is not a heuristic: every value a compiled body reads from its own
# frame is one it put there.
#
# WHY OFFLINE. The defect this was written for
# (`jit-warm-groupdata-window-row-collapse`) does not survive instrumentation --
# adding a counter to the Java method, or a Rust backtrace to the emitter,
# changes which methods tier up and the miscompile goes away. This reads a dump
# the existing `CRATONVM_DBG_JIT_DISASM` flag already produces, so it perturbs
# nothing at all.
#
# It found `Select.processGroupResult` reading `[rbp-0B0h]` -- three reads, zero
# writes -- which is the `long offset` parameter's loop-carried home. The
# garbage it read back was large and positive, so the loop's `quickOffset &&
# offset > 0` arm dropped result rows as if the query had an OFFSET clause.

import re,sys
# Report frame slots a dumped body READS and never WRITES.
txt=open(sys.argv[1],encoding='utf-8',errors='replace').read()
blocks=re.split(r'^\[cratonvm-jit-disasm\] ', txt, flags=re.M)[1:]
for b in blocks:
    head=b.split('\n',1)[0]
    body=b.split('\n',1)[1] if '\n' in b else ''
    writes=set(re.findall(r'(?:mov|movzx)\s+(?:qword |dword |byte )?\[(rbp-[0-9A-Fa-f]+h)\]\s*,', body))
    reads=set(re.findall(r',\s*\[(rbp-[0-9A-Fa-f]+h)\]', body))
    reads |= set(re.findall(r'cmp\s+\S+\s*,\s*\[(rbp-[0-9A-Fa-f]+h)\]', body))
    bad=sorted(reads-writes, key=lambda s:int(s[4:-1],16))
    name=head.split(' ')[1] if ' ' in head else head
    if bad:
        print("%-88s reads-but-never-writes: %s" % (name[:88], ' '.join(bad)))
    else:
        print("%-88s clean" % name[:88])
