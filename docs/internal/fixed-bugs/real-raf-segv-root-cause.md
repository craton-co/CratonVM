# Real RandomAccessFile JIT crash: root cause and closure

The historical real-`RandomAccessFile` crash was not evidence that real JDK
bytecode was inherently unsafe. It exposed two VM defects:

1. JIT/native transitions did not consistently publish all live object
   references to the collector.
2. cached compiled call targets could survive class/redefinition state changes
   that invalidated their assumptions.

Both defect classes now have targeted root-publication, cache-generation, and
real-RAF regression coverage. Real `RandomAccessFile` is the default path;
`CRATONVM_SYNTHETIC_RAF=1` exists only as a diagnostic compatibility fallback.

Any new native crash must be recorded under `docs/known-issues` with a minimal
reproducer. Do not reopen this historical umbrella without evidence that one of
the two mechanisms above has actually regressed.
