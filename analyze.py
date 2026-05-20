import struct, sys, bisect, glob

DUMP = r'C:\craton\CratonVM\hangdbg.dmp'
data = open(DUMP, 'rb').read()
print(f"dump size {len(data)}")

# --- header ---
sig, ver, nstreams, dir_rva = struct.unpack_from('<IIII', data, 0)
assert sig == 0x504D444D, hex(sig)

streams = {}
for i in range(nstreams):
    off = dir_rva + i*12
    stype, dsize, drva = struct.unpack_from('<III', data, off)
    streams.setdefault(stype, []).append((dsize, drva))
print("stream types:", sorted(streams.keys()))

# --- Memory64List (type 9): real memory for >4GB full dumps ---
mem_ranges = []  # (start, size, file_offset)
for dsize, drva in streams.get(9, []):
    nranges, base_rva = struct.unpack_from('<QQ', data, drva)
    off = base_rva
    for r in range(nranges):
        rstart, rsize = struct.unpack_from('<QQ', data, drva + 16 + r*16)
        mem_ranges.append((rstart, rsize, off))
        off += rsize
mem_ranges.sort()
mr_starts = [m[0] for m in mem_ranges]
print(f"memory64 ranges: {len(mem_ranges)}")

def read_mem(addr, n):
    i = bisect.bisect_right(mr_starts, addr) - 1
    if i < 0:
        return None
    rstart, rsize, foff = mem_ranges[i]
    if not (rstart <= addr and addr + n <= rstart + rsize):
        return None
    fo = foff + (addr - rstart)
    return data[fo:fo+n]

# --- modules (type 4) ---
modules = []
for dsize, drva in streams.get(4, []):
    nmods, = struct.unpack_from('<I', data, drva)
    for m in range(nmods):
        # MINIDUMP_MODULE = 108 bytes
        mo = drva + 4 + m*108
        base, size = struct.unpack_from('<QI', data, mo)
        name_rva, = struct.unpack_from('<I', data, mo+20)
        nlen, = struct.unpack_from('<I', data, name_rva)
        name = data[name_rva+4:name_rva+4+nlen].decode('utf-16-le', 'replace')
        name = name.replace('\\','/').split('/')[-1]
        modules.append((base, size, name))
modules.sort()
for b,s,n in modules:
    print(f"  module {b:#018x} +{s:#x} {n}")

def mod_for(addr):
    for b,s,n in modules:
        if b <= addr < b+s:
            return (n, addr-b)
    return (None, None)

# --- threads (type 3) ---
threads = []
for dsize, drva in streams.get(3, []):
    nthreads, = struct.unpack_from('<I', data, drva)
    for t in range(nthreads):
        to = drva + 4 + t*48
        tid, susp, prio_cls, prio, teb = struct.unpack_from('<IIIIQ', data, to)
        stk_start, stk_dsize, stk_rva = struct.unpack_from('<QII', data, to+24)
        ctx_dsize, ctx_rva = struct.unpack_from('<II', data, to+40)
        # CONTEXT amd64: rsp @0x98, rbp @0xA0, rip @0xF8
        rsp = struct.unpack_from('<Q', data, ctx_rva+0x98)[0]
        rbp = struct.unpack_from('<Q', data, ctx_rva+0xA0)[0]
        rip = struct.unpack_from('<Q', data, ctx_rva+0xF8)[0]
        threads.append(dict(tid=tid, rsp=rsp, rbp=rbp, rip=rip,
                            stk_start=stk_start, stk_size=stk_dsize, stk_rva=stk_rva))

# --- symbol resolver from breakpad .sym ---
symfiles = glob.glob(r'C:\craton\CratonVM\symbols-dbg/cratonvm.pdb/*/cratonvm.sym')
funcs = []  # (rva, size, name)
pubs = []   # (rva, name)
if symfiles:
    for line in open(symfiles[0], 'r', encoding='utf-8', errors='replace'):
        if line.startswith('FUNC '):
            p = line.split(maxsplit=5)
            idx = 2 if p[1]=='m' else 1
            addr=int(p[idx],16); size=int(p[idx+1],16)
            name=p[idx+3].rstrip() if len(p)>idx+3 else '?'
            funcs.append((addr,size,name))
        elif line.startswith('PUBLIC '):
            p = line.split(maxsplit=4)
            idx = 2 if p[1]=='m' else 1
            addr=int(p[idx],16)
            name=p[idx+2].rstrip() if len(p)>idx+2 else '?'
            pubs.append((addr,name))
funcs.sort()
faddrs=[f[0] for f in funcs]
pubs.sort()
paddrs=[p[0] for p in pubs]
print(f"loaded {len(funcs)} FUNC, {len(pubs)} PUBLIC symbols")

def resolve(addr):
    n, off = mod_for(addr)
    if n is None:
        return f"{addr:#018x} ???"
    if n.lower() != 'cratonvm.exe':
        return f"{n}+{off:#x}"
    i = bisect.bisect_right(faddrs, off) - 1
    if i >= 0:
        a,s,name = funcs[i]
        if off < a+s:
            return f"{name}  (+{off-a:#x})"
    j = bisect.bisect_right(paddrs, off) - 1
    if j >= 0:
        a,name = pubs[j]
        return f"~{name}  (+{off-a:#x})  [PUBLIC,rustjvm+{off:#x}]"
    return f"rustjvm.exe+{off:#x} (no sym)"

rustjvm_base = [b for b,s,n in modules if n.lower()=='cratonvm.exe'][0]
rustjvm_end  = rustjvm_base + [s for b,s,n in modules if n.lower()=='cratonvm.exe'][0]

print("\n=== threads ===")
for th in threads:
    n,off = mod_for(th['rip'])
    print(f"tid {th['tid']:>6}  rip={th['rip']:#018x} ({resolve(th['rip'])})  rsp={th['rsp']:#018x} stk[{th['stk_start']:#x}+{th['stk_size']:#x}]")

# focus: print return-address-like qwords on each thread stack
for th in threads:
    print(f"\n=== tid {th['tid']} stack scan (rustjvm.exe return addrs, from rsp up) ===")
    rsp = th['rsp']
    # find the memory64 range containing rsp -> that's the live stack
    i = bisect.bisect_right(mr_starts, rsp) - 1
    if i < 0:
        print("  no mem range"); continue
    rstart, rsize, foff = mem_ranges[i]
    if not (rstart <= rsp < rstart+rsize):
        print(f"  rsp {rsp:#x} not in any mem range"); continue
    print(f"  stack region [{rstart:#x}+{rsize:#x}], scanning {rsp:#x}..{rstart+rsize:#x}")
    seen=0
    for addr in range((rsp & ~7), rstart+rsize-8, 8):
        b = data[foff+(addr-rstart):foff+(addr-rstart)+8]
        val = struct.unpack('<Q', b)[0]
        if rustjvm_base <= val < rustjvm_end:
            print(f"  [{addr:#018x}] -> {resolve(val)}")
            seen+=1
        if seen>250:
            print("  ...truncated"); break
