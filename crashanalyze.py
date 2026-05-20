import struct, bisect, capstone

DUMP = r'C:\craton\CratonVM\crash.dmp'
data = open(DUMP, 'rb').read()
sig, ver, nstreams, dir_rva = struct.unpack_from('<IIII', data, 0)
streams = {}
for i in range(nstreams):
    stype, dsize, drva = struct.unpack_from('<III', data, dir_rva + i*12)
    streams.setdefault(stype, []).append((dsize, drva))

mem_ranges = []
for dsize, drva in streams.get(9, []):
    nranges, base_rva = struct.unpack_from('<QQ', data, drva)
    off = base_rva
    for r in range(nranges):
        rstart, rsize = struct.unpack_from('<QQ', data, drva + 16 + r*16)
        mem_ranges.append((rstart, rsize, off))
        off += rsize
mem_ranges.sort()
mr_starts = [m[0] for m in mem_ranges]

def read_mem(addr, n):
    i = bisect.bisect_right(mr_starts, addr) - 1
    if i < 0: return None
    rstart, rsize, foff = mem_ranges[i]
    if not (rstart <= addr and addr + n <= rstart + rsize): return None
    fo = foff + (addr - rstart)
    return data[fo:fo+n]

# threads
threads = []
for dsize, drva in streams.get(3, []):
    nthreads, = struct.unpack_from('<I', data, drva)
    for t in range(nthreads):
        to = drva + 4 + t*48
        tid, = struct.unpack_from('<I', data, to)
        ctx_dsize, ctx_rva = struct.unpack_from('<II', data, to+40)
        regs = {}
        for name,off in [('rax',0x78),('rcx',0x80),('rdx',0x88),('rbx',0x90),
                          ('rsp',0x98),('rbp',0xA0),('rsi',0xA8),('rdi',0xB0),
                          ('r8',0xB8),('r9',0xC0),('r10',0xC8),('r11',0xD0),
                          ('r12',0xD8),('r13',0xE0),('r14',0xE8),('r15',0xF0),
                          ('rip',0xF8)]:
            regs[name] = struct.unpack_from('<Q', data, ctx_rva+off)[0]
        threads.append((tid, regs))

for tid, regs in threads:
    print(f"tid {tid} rip={regs['rip']:#x}")
    if 0x1000000 <= regs['rip'] < 0x80000000:  # JIT'd code low region
        print(f"  *** FAULTING THREAD (JIT code) ***")
        for k in ('rax','rcx','rdx','rbx','rsp','rbp','rsi','rdi','r8','r9','r10','r11','r12','r13','r14','r15'):
            print(f"    {k}={regs[k]:#018x}")

# disassemble around the crash rip
CRASH_RIP = 0x14300b0
for tid, regs in threads:
    if regs['rip'] == CRASH_RIP:
        crash_regs = regs
        break
else:
    crash_regs = None

md = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
start = CRASH_RIP - 0x80
code = read_mem(start, 0x140)
print(f"\n=== disasm around crash rip {CRASH_RIP:#x} ===")
if code:
    for ins in md.disasm(code, start):
        mark = '  >>> CRASH' if ins.address == CRASH_RIP else ''
        print(f"  {ins.address:#010x}: {ins.mnemonic:<10} {ins.op_str}{mark}")
else:
    print("  crash rip region not in dump")

# also find the JIT code region containing the crash
for rstart, rsize, foff in mem_ranges:
    if rstart <= CRASH_RIP < rstart+rsize:
        print(f"\ncrash rip in mem range [{rstart:#x}+{rsize:#x}]")
        break
