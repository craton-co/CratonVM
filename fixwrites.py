import os, re
pats = [
 # compound flag updates -> atomic helpers
 (re.compile(r"([\w\.\(\)\*\[\]&]+?)\.gc_flags\(\)\s*\|=\s*([^;]+);"), r"\1.add_gc_flags(\2);"),
 (re.compile(r"([\w\.\(\)\*\[\]&]+?)\.gc_flags\(\)\s*&=\s*!\s*\(([^;]+)\);"), r"\1.clear_gc_flags(\2);"),
 (re.compile(r"([\w\.\(\)\*\[\]&]+?)\.gc_flags\(\)\s*&=\s*!\s*([A-Z_0-9]+);"), r"\1.clear_gc_flags(\2);"),
 (re.compile(r"([\w\.\(\)\*\[\]&]+?)\.gc_flags\(\)\s*=\s*([^;]+);"), r"\1.set_gc_flags(\2);"),
 # age
 (re.compile(r"([\w\.\(\)\*\[\]&]+?)\.gc_age\(\)\s*\+=\s*1;"), r"\1.set_gc_age(\1.gc_age() + 1);"),
 (re.compile(r"([\w\.\(\)\*\[\]&]+?)\.gc_age\(\)\s*=\s*([^;]+);"), r"\1.set_gc_age(\2);"),
 # addr_of! on an accessor is a temporary; the value is what was wanted
 (re.compile(r"std::ptr::addr_of!\(\(\*(\w+)\)\.(kind|element_type|gc_flags|gc_age)\(\)\)\s*\.read_unaligned\(\)"),
  r"(*\1).\2()"),
 (re.compile(r"std::ptr::addr_of!\(\(\*(\w+)\)\.(kind|element_type|gc_flags|gc_age)\(\)\)\s*\.read\(\)"),
  r"(*\1).\2()"),
]
n=0
for root in ["gc/src","vm/src","jit/src","types/src","native-builtins/src","native-collections/src","native-io/src"]:
    for dp,_,fs in os.walk(root):
        for f in fs:
            if not f.endswith(".rs"): continue
            p=os.path.join(dp,f).replace(os.sep,"/")
            s=open(p,'rb').read().decode('utf-8'); o=s
            for pat,rep in pats: s=pat.sub(rep,s)
            if s!=o:
                open(p,'wb').write(s.encode('utf-8')); n+=1
print("files rewritten:", n)
