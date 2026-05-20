import struct, bisect, sys, capstone

DUMP = r'C:\craton\CratonVM\hangfull.dmp'
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

RUST_BASE = 0x7ff7050d0000

md = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
md.detail = False

def disas(rva, n=80, label=''):
    addr = RUST_BASE + rva
    code = read_mem(addr, n)
    print(f"\n--- disas rustjvm+{rva:#x} ({label}) ---")
    if code is None:
        print("  <not in dump>"); return
    for ins in md.disasm(code, addr):
        rel = ins.address - RUST_BASE
        tgt = ''
        if ins.mnemonic in ('call','jmp') or ins.mnemonic.startswith('j'):
            try:
                t = int(ins.op_str, 16)
                tgt = f"   => rustjvm+{t-RUST_BASE:#x}"
            except ValueError:
                pass
        print(f"  +{rel:#08x}: {ins.mnemonic:<8} {ins.op_str}{tgt}")

for rva,label,n in [(0xf95580,'F (0xf95598 caller)',0xb0),
                   (0xdbce80,'G get_or_init (0xdbcf6f)',0x280),
                   (0xc34300,'0xc3438d region',0xc0),
                   (0xc31f80,'0xc31fb7 region',0xa0)]:
    disas(rva, n, label)
