import io, sys
n = 0
P = '/data/cvm-pmv-20260827/types/src/flag_groups.rs'
s = io.open(P, encoding='utf-8', newline='').read()
A = '    E { group: Group::JIT, token: "receiver-despec", on_key: Some("CRATONVM_JIT_RECEIVER_DESPEC"), off_key: None, off_word: Some("0") },\n'
B = '    E { group: Group::JIT, token: "receiver-despec", on_key: Some("CRATONVM_JIT_RECEIVER_DESPEC"), off_key: None, off_word: Some("0") },\n    E { group: Group::JIT, token: "spill-narrow", on_key: Some("CRATONVM_JIT_SPILL_NARROW"), off_key: None, off_word: Some("0") },\n'
assert s.count(A) == 1, ('anchor count', s.count(A))
io.open(P, 'w', encoding='utf-8', newline='').write(s.replace(A, B)); n += 1
F = '/data/cvm-pmv-20260827/types/tests/flag-surface.txt'
body = [x for x in io.open(F, encoding='utf-8', newline='').read().split(chr(10)) if x.strip()]
t = 'CRATONVM_JIT_SPILL_NARROW'
if t not in body: body.append(t); n += 1
body.sort()
io.open(F, 'w', encoding='utf-8', newline='').write(chr(10).join(body) + chr(10))
print('patched:', n)
