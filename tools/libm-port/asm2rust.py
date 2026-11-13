#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
"""Translate one Intel LIBM x86-64 routine (AT&T syntax, as Android bionic
publishes it under BSD-3-Clause, e.g. `libm/x86_64/e_pow.S` at tag
android-10.0.0_r1) into a Rust function that performs the same instructions on
an explicit register file, so it returns the routine's bits by construction.

Usage: asm2rust.py <e_pow.S> <entry> <rust-fn-name> > out.rs

The generated module needs the runtime in `types/src/intel_libm.rs` (the
`Cpu` type). Supported: exactly the instruction forms the routine uses; any
other form stops the translation with its line number, so a new routine
cannot be mistranslated silently.
"""
import re
import sys

GPR = {}
for i, (q, d, w) in enumerate([
        ('rax', 'eax', 'ax'), ('rcx', 'ecx', 'cx'), ('rdx', 'edx', 'dx'), ('rbx', 'ebx', 'bx'),
        ('rsp', 'esp', 'sp'), ('rbp', 'ebp', 'bp'), ('rsi', 'esi', 'si'), ('rdi', 'edi', 'di')]):
    GPR[q] = (i, 64)
    GPR[d] = (i, 32)
    GPR[w] = (i, 16)
for i, b in enumerate(['al', 'cl', 'dl', 'bl']):
    GPR[b] = (i, 8)
for i in range(8, 16):
    GPR['r%d' % i] = (i, 64)
    GPR['r%dd' % i] = (i, 32)
    GPR['r%dw' % i] = (i, 16)


def die(msg):
    sys.stderr.write(msg + '\n')
    sys.exit(2)


class Operand:
    pass


def parse_operand(text, symbols, lineno):
    t = text.strip()
    o = Operand()
    if t.startswith('$'):
        o.kind = 'imm'
        o.value = int(t[1:], 0)
        return o
    if t.startswith('%xmm'):
        o.kind = 'xmm'
        o.index = int(t[4:])
        return o
    if t.startswith('%'):
        if t[1:] not in GPR:
            die('line %d: unknown register %s' % (lineno, t))
        o.kind = 'gpr'
        o.index, o.width = GPR[t[1:]]
        return o
    return parse_memory(t.replace(' ', ''), symbols, lineno)


def parse_memory(t, symbols, lineno):
    """`[disp+]SYM(%rip)` or `[disp](%base[,%index[,scale]])`."""
    o = Operand()
    o.kind = 'mem'
    m = re.fullmatch(r'(-?\d+)?\+?([A-Za-z_][A-Za-z_0-9]*)?\(%rip\)', t)
    if m and m.group(2):
        if m.group(2) not in symbols:
            die('line %d: unknown symbol %s' % (lineno, m.group(2)))
        o.sym, o.disp, o.base, o.idx, o.scale = m.group(2), int(m.group(1) or 0), None, None, 1
        return o
    m = re.fullmatch(r'(-?\d+)?\(%([a-z0-9]+)(?:,%([a-z0-9]+)(?:,(\d))?)?\)', t)
    if not m:
        die('line %d: unsupported operand %r' % (lineno, t))
    o.sym = None
    o.disp = int(m.group(1) or 0)
    o.base = GPR[m.group(2)][0]
    o.idx = GPR[m.group(3)][0] if m.group(3) else None
    o.scale = int(m.group(4) or 1)
    return o


def split_operands(s):
    out, depth, cur = [], 0, ''
    for ch in s:
        if ch == '(':
            depth += 1
        elif ch == ')':
            depth -= 1
        if ch == ',' and depth == 0:
            out.append(cur)
            cur = ''
        else:
            cur += ch
    if cur.strip():
        out.append(cur)
    return out


def addr(o):
    """A Rust expression for the operand's address."""
    parts = []
    if o.sym is not None:
        parts.append('SYM_%s' % o.sym.upper())
    if o.base is not None:
        parts.append('c.g[%d]' % o.base)
    if o.idx is not None:
        parts.append('c.g[%d].wrapping_mul(%d)' % (o.idx, o.scale))
    expr = parts[0]
    for p in parts[1:]:
        expr = '%s.wrapping_add(%s)' % (expr, p)
    if o.disp:
        expr = '%s.wrapping_add(%du64)' % (expr, o.disp & 0xFFFFFFFFFFFFFFFF)
    return expr


def r32(o):
    return '(c.g[%d] as u32)' % o.index


class Ins:
    """One instruction being translated: its operands and the shared emitters."""

    def __init__(self, m, ops, symbols, lineno):
        self.m = m
        self.ops = ops
        self.lineno = lineno
        self.o = [parse_operand(x, symbols, lineno) for x in ops]
        self.k = [x.kind for x in self.o]

    def bad(self):
        die('line %d: unsupported form %s %s' % (self.lineno, self.m, ', '.join(self.ops)))

    @staticmethod
    def w(x):
        return getattr(x, 'width', None)

    # Memory: the routine's frame (`disp(%rsp)`, after its one
    # `subq $40, %rsp`) or its read-only image (a symbol, or a register that
    # `lea` pointed into it).
    @staticmethod
    def is_frame(x):
        return x.kind == 'mem' and x.base == 4

    def frame_disp(self, x):
        if x.idx is not None or x.sym is not None or not (0 <= x.disp <= 40):
            self.bad()
        return x.disp

    def ld(self, x, n):
        if self.is_frame(x):
            return 'c.frame_ld::<%d>(%d)' % (n, self.frame_disp(x))
        return 'image_ld::<%d>(&IMAGE, %s)' % (n, addr(x))

    def ld128(self, x):
        if self.is_frame(x):
            d = self.frame_disp(x)
            return '_mm_set_epi64x(c.frame_ld::<8>(%d) as i64, c.frame_ld::<8>(%d) as i64)' % (d + 8, d)
        return 'image_ld128(&IMAGE, %s)' % addr(x)

    def st(self, x, n, val):
        if not self.is_frame(x):
            self.bad()
        return 'c.frame_st::<%d>(%d, %s);' % (n, self.frame_disp(x), val)

    def xsrc(self, x):
        """A 128-bit source operand."""
        return self.ld128(x) if x.kind == 'mem' else 'c.x[%d]' % x.index

    def need(self, kinds):
        """Stop unless the operand kinds are exactly `kinds`."""
        if self.k != kinds:
            self.bad()


# ---- moves

def h_movl(t):
    o, k, w = t.o, t.k, t.w
    if k == ['imm', 'gpr'] and w(o[1]) == 32:
        return 'c.g[%d] = %du64;' % (o[1].index, o[0].value & 0xFFFFFFFF)
    if k == ['gpr', 'gpr'] and w(o[0]) == 32 and w(o[1]) == 32:
        return 'c.g[%d] = %s as u64;' % (o[1].index, r32(o[0]))
    if k == ['imm', 'mem']:
        return t.st(o[1], 4, '%du64' % (o[0].value & 0xFFFFFFFF))
    if k == ['mem', 'gpr'] and w(o[1]) == 32:
        return 'c.g[%d] = %s;' % (o[1].index, t.ld(o[0], 4))
    t.bad()


def h_mov64(t):
    """`movq` / `movd` / `movsd`: one 64-bit lane (or 32 bits for `movd r32`),
    by operand kinds."""
    route = {('imm', 'gpr'): h_mov_imm64, ('mem', 'xmm'): h_mov_load64, ('xmm', 'mem'): h_mov_store64,
             ('xmm', 'xmm'): h_mov_xmm_xmm, ('gpr', 'xmm'): h_mov_gpr_xmm, ('xmm', 'gpr'): h_mov_xmm_gpr}
    handler = route.get(tuple(t.k))
    if handler is None:
        t.bad()
    return handler(t)


def h_mov_imm64(t):
    o = t.o
    if t.m != 'movq' or t.w(o[1]) != 64:
        t.bad()
    return 'c.g[%d] = 0x%016xu64;' % (o[1].index, o[0].value & 0xFFFFFFFFFFFFFFFF)


def h_mov_load64(t):
    if t.m == 'movd':
        t.bad()
    return 'c.x[%d] = from64(%s);' % (t.o[1].index, t.ld(t.o[0], 8))


def h_mov_store64(t):
    if t.m == 'movd':
        t.bad()
    return t.st(t.o[1], 8, 'lo64(c.x[%d])' % t.o[0].index)


def h_mov_xmm_xmm(t):
    o = t.o
    if t.m == 'movq':
        return 'c.x[%d] = _mm_move_epi64(c.x[%d]);' % (o[1].index, o[0].index)
    if t.m == 'movsd':
        return ('c.x[%d] = _mm_castpd_si128(_mm_move_sd(_mm_castsi128_pd(c.x[%d]), '
                '_mm_castsi128_pd(c.x[%d])));') % (o[1].index, o[1].index, o[0].index)
    t.bad()


def h_mov_gpr_xmm(t):
    o = t.o
    if t.m == 'movsd':
        t.bad()
    if t.w(o[0]) == 64:
        return 'c.x[%d] = from64(c.g[%d]);' % (o[1].index, o[0].index)
    if t.w(o[0]) == 32:
        return 'c.x[%d] = _mm_cvtsi32_si128(%s as i32);' % (o[1].index, r32(o[0]))
    t.bad()


def h_mov_xmm_gpr(t):
    o = t.o
    if t.m == 'movsd':
        t.bad()
    if t.w(o[1]) == 64:
        return 'c.g[%d] = lo64(c.x[%d]);' % (o[1].index, o[0].index)
    if t.w(o[1]) == 32:
        return 'c.g[%d] = u64::from(_mm_cvtsi128_si32(c.x[%d]) as u32);' % (o[1].index, o[0].index)
    t.bad()


def h_mov128(t):
    o, k = t.o, t.k
    if k == ['mem', 'xmm']:
        return 'c.x[%d] = %s;' % (o[1].index, t.ld128(o[0]))
    t.need(['xmm', 'xmm'])
    return 'c.x[%d] = c.x[%d];' % (o[1].index, o[0].index)


def h_movw(t):
    o = t.o
    t.need(['mem', 'gpr'])
    if t.w(o[1]) != 16:
        t.bad()
    return 'c.g[%d] = (c.g[%d] & !0xFFFF) | %s;' % (o[1].index, o[1].index, t.ld(o[0], 2))


def h_lea(t):
    o = t.o
    t.need(['mem', 'gpr'])
    if t.w(o[1]) != 64 or t.is_frame(o[0]):
        t.bad()
    return 'c.g[%d] = %s;' % (o[1].index, addr(o[0]))


# ---- 128-bit integer / logic: the SSE2 intrinsic of the same instruction

# Lane-wise two-operand ops: mnemonic -> the SSE2 intrinsic's name.
LANE_BINOPS = {'andpd': 'and_si128', 'andps': 'and_si128', 'pand': 'and_si128',
               'orpd': 'or_si128', 'orps': 'or_si128', 'por': 'or_si128',
               'xorpd': 'xor_si128', 'xorps': 'xor_si128', 'pxor': 'xor_si128',
               'paddd': 'add_epi32', 'paddq': 'add_epi64', 'psubd': 'sub_epi32', 'psubq': 'sub_epi64'}
SHIFTS = {'psrlq': 'srli_epi64', 'psllq': 'slli_epi64', 'psrld': 'srli_epi32', 'pslld': 'slli_epi32'}


def h_lane_binop(t):
    o = t.o
    if t.k[1] != 'xmm':
        t.bad()
    return 'c.x[%d] = _mm_%s(c.x[%d], %s);' % (o[1].index, LANE_BINOPS[t.m], o[1].index, t.xsrc(o[0]))


def h_shift(t):
    o = t.o
    t.need(['imm', 'xmm'])
    return 'c.x[%d] = _mm_%s::<%d>(c.x[%d]);' % (o[1].index, SHIFTS[t.m], o[0].value, o[1].index)


def h_pshufd(t):
    o = t.o
    t.need(['imm', 'xmm', 'xmm'])
    return 'c.x[%d] = _mm_shuffle_epi32::<%d>(c.x[%d]);' % (o[2].index, o[0].value, o[1].index)


def h_pextrw(t):
    o = t.o
    t.need(['imm', 'xmm', 'gpr'])
    return 'c.g[%d] = u64::from(_mm_extract_epi16::<%d>(c.x[%d]) as u32);' % (o[2].index, o[0].value & 7, o[1].index)


def h_pinsrw(t):
    o = t.o
    t.need(['imm', 'gpr', 'xmm'])
    return 'c.x[%d] = _mm_insert_epi16::<%d>(c.x[%d], c.g[%d] as i32);' % (o[2].index, o[0].value & 7, o[2].index, o[1].index)


def h_unpcklpd(t):
    o = t.o
    if t.k[1] != 'xmm':
        t.bad()
    return 'c.x[%d] = _mm_unpacklo_epi64(c.x[%d], %s);' % (o[1].index, o[1].index, t.xsrc(o[0]))


# ---- floating point: the instruction itself (`intel_libm::sse`). Rust
# leaves NaN bits unspecified and LLVM folds and commutes float ops, so a
# NaN the routine makes (x86's default NaN fff8...) or passes on (the
# destination operand's) would not be the routine's otherwise.

def h_rcpps(t):
    o = t.o
    t.need(['xmm', 'xmm'])
    return 'c.x[%d] = sse::rcpps(c.x[%d]);' % (o[1].index, o[0].index)


def h_scalar_double(t):
    o = t.o
    if t.k[1] != 'xmm':
        t.bad()
    src = 'from64(%s)' % t.ld(o[0], 8) if t.k[0] == 'mem' else 'c.x[%d]' % o[0].index
    return 'c.x[%d] = sse::%s(c.x[%d], %s);' % (o[1].index, t.m, o[1].index, src)


def h_packed_double(t):
    o = t.o
    if t.k[1] != 'xmm':
        t.bad()
    return 'c.x[%d] = sse::%s(c.x[%d], %s);' % (o[1].index, t.m, o[1].index, t.xsrc(o[0]))


def h_scalar_single(t):
    o = t.o
    t.need(['xmm', 'xmm'])
    return 'c.x[%d] = sse::%s(c.x[%d], c.x[%d]);' % (o[1].index, t.m, o[1].index, o[0].index)


def h_cvtsi2sd(t):
    o = t.o
    t.need(['gpr', 'xmm'])
    if t.w(o[0]) != 32:
        t.bad()
    return 'c.x[%d] = sse::cvtsi2sd32(c.x[%d], %s);' % (o[1].index, o[1].index, r32(o[0]))


# ---- integer ALU with the flags the branches read

ALU32 = {'addl': 'add32', 'subl': 'sub32', 'andl': 'and32', 'orl': 'or32', 'xorl': 'xor32',
         'cmpl': 'sub32', 'testl': 'and32'}
SHIFT32 = {'shll': 'shl32', 'shrl': 'shr32', 'sarl': 'sar32'}


def h_alu32(t):
    o, k = t.o, t.k
    if k[1] != 'gpr' or t.w(o[1]) != 32 or k[0] not in ('imm', 'gpr'):
        t.bad()
    src = '%du32' % (o[0].value & 0xFFFFFFFF) if k[0] == 'imm' else r32(o[0])
    f = ALU32[t.m]
    if t.m in ('cmpl', 'testl'):
        return 'let _ = c.%s(%s, %s);' % (f, r32(o[1]), src)
    return 'c.g[%d] = c.%s(%s, %s) as u64;' % (o[1].index, f, r32(o[1]), src)


def h_alu16(t):
    o = t.o
    t.need(['imm', 'gpr'])
    if t.w(o[1]) != 16:
        t.bad()
    dst = '(c.g[%d] as u16)' % o[1].index
    if t.m == 'cmpw':
        return 'let _ = c.sub16(%s, %du16);' % (dst, o[0].value & 0xFFFF)
    return 'c.g[%d] = (c.g[%d] & !0xFFFF) | c.and16(%s, %du16) as u64;' % (o[1].index, o[1].index, dst, o[0].value & 0xFFFF)


def h_shift32(t):
    o, k = t.o, t.k
    if k[1] != 'gpr' or t.w(o[1]) != 32:
        t.bad()
    if k[0] == 'imm':
        n = '%du32' % o[0].value
    elif k[0] == 'gpr' and o[0].index == 1:  # %cl
        n = '(c.g[1] as u32)'
    else:
        t.bad()
    return 'c.g[%d] = c.%s(%s, %s) as u64;' % (o[1].index, SHIFT32[t.m], r32(o[1]), n)


def h_addsub64(t):
    """`addq` / `subq`: the frame's one reserve/release pair emits nothing."""
    o = t.o
    t.need(['imm', 'gpr'])
    if o[1].index == 4:
        if o[0].value != 40:
            t.bad()
        return '// frame %s' % ('released' if t.m == 'addq' else 'reserved')
    if t.w(o[1]) != 64:
        t.bad()
    f = 'wrapping_add' if t.m == 'addq' else 'wrapping_sub'
    return 'c.g[%d] = c.g[%d].%s(%du64);' % (o[1].index, o[1].index, f, o[0].value & 0xFFFFFFFFFFFFFFFF)


def h_shlq(t):
    o = t.o
    t.need(['imm', 'gpr'])
    if t.w(o[1]) != 64:
        t.bad()
    return 'c.g[%d] <<= %d;' % (o[1].index, o[0].value & 63)


def h_bsr(t):
    o = t.o
    t.need(['gpr', 'gpr'])
    if t.w(o[0]) != 32 or t.w(o[1]) != 32:
        t.bad()
    return 'c.bsr32(%d, %d);' % (o[0].index, o[1].index)


HANDLERS = {'movl': h_movl, 'movq': h_mov64, 'movd': h_mov64, 'movsd': h_mov64, 'movw': h_movw,
            'lea': h_lea, 'pshufd': h_pshufd, 'pextrw': h_pextrw, 'pinsrw': h_pinsrw,
            'unpcklpd': h_unpcklpd, 'rcpps': h_rcpps, 'cvtsi2sd': h_cvtsi2sd, 'andw': h_alu16,
            'cmpw': h_alu16, 'addq': h_addsub64, 'subq': h_addsub64, 'shlq': h_shlq, 'bsr': h_bsr}
for _m in ('movapd', 'movaps', 'movdqa', 'movups', 'movupd', 'movdqu'):
    HANDLERS[_m] = h_mov128
for _table, _h in ((LANE_BINOPS, h_lane_binop), (SHIFTS, h_shift), (ALU32, h_alu32),
                   (SHIFT32, h_shift32)):
    for _m in _table:
        HANDLERS[_m] = _h
for _m in ('addsd', 'subsd', 'mulsd', 'divsd'):
    HANDLERS[_m] = h_scalar_double
for _m in ('addpd', 'subpd', 'mulpd', 'divpd'):
    HANDLERS[_m] = h_packed_double
for _m in ('mulss', 'addss', 'subss'):
    HANDLERS[_m] = h_scalar_single


def translate(ins, ops, symbols, lineno, labels):
    """Return Rust statements for one instruction."""
    if ins.startswith('j') or ins == 'ret':
        return translate_flow(ins, ops, lineno, labels)
    t = Ins(ins, ops, symbols, lineno)
    handler = HANDLERS.get(ins)
    if handler is None:
        t.bad()
    return handler(t)


def translate_flow(m, ops, lineno, labels):
    """A jump or `ret`."""
    jcc = {'je': 'c.zf', 'jne': '!c.zf', 'ja': '!c.cf && !c.zf', 'jae': '!c.cf', 'jb': 'c.cf',
           'jbe': 'c.cf || c.zf', 'jl': 'c.sf != c.of', 'jle': 'c.zf || c.sf != c.of',
           'jg': '!c.zf && c.sf == c.of', 'jge': 'c.sf == c.of', 'js': 'c.sf', 'jns': '!c.sf'}
    if m in jcc:
        target = ops[0].strip()
        if target not in labels:
            die('line %d: unknown label %s' % (lineno, target))
        return 'if %s { blk = %d; continue; }' % (jcc[m], labels[target])
    if m == 'jmp':
        target = ops[0].strip()
        if target not in labels:
            die('line %d: unknown label %s' % (lineno, target))
        return 'blk = %d; continue;' % labels[target]
    if m == 'ret':
        return 'return f64::from_bits(lo64(c.x[0]));'
    die('line %d: unsupported jump %s' % (lineno, m))


def data_bytes(line):
    """The bytes one data directive lays down, or `None` for a directive that
    lays down none (`.type`, `.size`, sections)."""
    if line.startswith('.long'):
        return (int(line.split()[1], 0) & 0xFFFFFFFF).to_bytes(4, 'little')
    if line.startswith('.quad'):
        return (int(line.split()[1], 0) & 0xFFFFFFFFFFFFFFFF).to_bytes(8, 'little')
    if line.startswith('.space'):
        args = [a.strip() for a in line.split(None, 1)[1].split(',')]
        fill = int(args[1], 0) if len(args) > 1 else 0
        return bytes([fill & 0xFF]) * int(args[0], 0)
    if line.startswith(('.type', '.size', '.section', '.data', '.text')):
        return None
    die('unsupported data line %r' % line)


def parse_data(lines):
    """The .rodata symbols in order: (name, align, bytes)."""
    syms = []
    pending_align = 1
    for raw in lines:
        line = raw.split('#')[0].strip()
        if not line:
            continue
        if line.startswith('.align'):
            pending_align = max(pending_align, int(line.split()[1]))
            continue
        m = re.fullmatch(r'([A-Za-z_][A-Za-z_0-9]*):', line)
        if m:
            syms.append([m.group(1), pending_align, bytearray()])
            pending_align = 1
            continue
        data = data_bytes(line)
        if data is not None:
            syms[-1][2] += data
    return syms


def read_code(text, start, end):
    """The routine's labels (block starts, numbered from 1; block 0 is the
    entry) and instructions, in order."""
    body = []
    labels = {}
    for i in range(start + 1, end):
        s = text[i].split('#')[0].strip()
        if not s:
            continue
        if s.endswith(':'):
            if not s.startswith('..___tag_value'):
                labels[s[:-1]] = len(labels) + 1
                body.append(('label', s[:-1]))
            continue
        parts = s.split(None, 1)
        body.append(('ins', parts[0], split_operands(parts[1]) if len(parts) > 1 else [], i + 1))
    return body, labels


def emit_header(text, source, entry):
    """Intel's licence block, verbatim, as line comments (BSD-3-Clause requires
    the notice to travel with any redistribution of the source), then the
    provenance line and the imports."""
    out = ['// SPDX-License-Identifier: BSD-3-Clause']
    lic_start = next(i for i, l in enumerate(text) if l.strip().startswith('/*'))
    lic_end = next(i for i, l in enumerate(text) if i >= lic_start and l.strip().endswith('*/'))
    for l in text[lic_start:lic_end + 1]:
        if l.strip() not in ('/*', '*/'):
            out.append(('// ' + l.rstrip()).rstrip())
    out.append('//')
    out.append('// Generated by tools/libm-port/asm2rust.py from %s (%s); do not edit by hand.' % (source, entry))
    out.append('// Every instruction of the routine is one statement below, its source line cited.')
    out.append('')
    out.append('use super::*;')
    out.append('use core::arch::x86_64::*;')
    return out


def emit_image(syms):
    """The read-only data as one byte image, and a constant per symbol."""
    image = bytearray()
    offsets = {}
    for name, align, data in syms:
        while len(image) % align:
            image.append(0)
        offsets[name] = len(image)
        image += data
    out = ['', '/// The routine\'s read-only data, laid out as the assembler lays it out.',
           'static IMAGE: [u8; %d] = [' % len(image)]
    for j in range(0, len(image), 16):
        out.append('    ' + ' '.join('0x%02x,' % b for b in image[j:j + 16]))
    out.append('];')
    for name in sorted(offsets):
        out.append('const SYM_%s: u64 = IMAGE_BASE + %d;' % (name.upper(), offsets[name]))
    return out


def emit_blocks(body, labels, symbols):
    """The match arms: one per block, falling through to the next unless the
    block ends in a jump or `ret`."""
    out = ['            0 => {']
    terminated = False
    for item in body:
        if item[0] == 'label':
            n = labels[item[1]]
            if not terminated:
                out.append('                blk = %d;' % n)
            out.append('            }')
            out.append('            %d => { // %s' % (n, item[1]))
            terminated = False
            continue
        _, ins, ops, lineno = item
        if terminated:
            die('line %d: unreachable instruction after a jump' % lineno)
        stmt = translate(ins, ops, symbols, lineno, labels)
        out.append('                %s // %d: %s %s' % (stmt, lineno, ins, ', '.join(o.strip() for o in ops)))
        terminated = ins in ('jmp', 'ret')
    if not terminated:
        out.append('                return f64::NAN;')
    out.append('            }')
    return out


def main():
    path, entry, fname = sys.argv[1], sys.argv[2], sys.argv[3]
    source = path.replace('\\', '/').split('/')[-1]
    text = open(path, encoding='utf-8').read().split('\n')
    start = next(i for i, l in enumerate(text) if l.strip() == 'ENTRY(%s)' % entry)
    end = next(i for i, l in enumerate(text) if l.strip() == 'END(%s)' % entry)
    rodata = next(i for i, l in enumerate(text) if l.strip().startswith('.section .rodata'))
    stop = next(i for i, l in enumerate(text) if i > rodata and l.strip().startswith('.section .note'))
    syms = parse_data(text[rodata + 1:stop])
    body, labels = read_code(text, start, end)

    out = emit_header(text, source, entry) + emit_image(syms)
    out.append('')
    out.append('/// `%s`, instruction for instruction (`%s` from %s).' % (fname, entry, source))
    out.append('#[allow(clippy::all, clippy::pedantic, unused_assignments, unused_mut, unused_unsafe)]')
    out.append('pub fn %s(x: f64, y: f64) -> f64 {' % fname)
    out.append('    let mut c = Cpu::new(x, y);')
    out.append('    let mut blk: u32 = 0;')
    out.append('    // SAFETY: SSE2 intrinsics, the x86-64 baseline; memory goes through the')
    out.append('    // checked frame and image loaders.')
    out.append('    unsafe { loop {')
    out.append('        match blk {')
    out += emit_blocks(body, labels, {s[0] for s in syms})
    out.append('            _ => return f64::NAN,')
    out.append('        }')
    out.append('    } }')
    out.append('}')
    # Bytes, so a Windows console never turns the newlines into CRLF.
    sys.stdout.buffer.write(('\n'.join(line.rstrip() for line in out) + '\n').encode('utf-8'))


if __name__ == '__main__':
    main()
