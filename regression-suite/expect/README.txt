Per-mode expected output for regression-suite vectors
=====================================================

Normally HotSpot is the oracle: run.sh runs each class on CratonVM and on
HotSpot and requires the extracted PASS/CK lines to be byte-identical. That is
correct for every vector whose right answer is the same in both compatibility
modes.

A vector whose CORRECT outcome differs between modes cannot use that oracle in
both. For those, drop a golden file here:

    <Class>.compatible.txt      expected output under the default --real-jdk
    <Class>.jdk-only.txt        expected output under --jdk-only

run.sh picks the file matching the active mode (derived from whether
CRATONVM_ARGS names --jdk-only). When a matching file exists it is
AUTHORITATIVE and REPLACES the HotSpot diff for that class. When it does not —
the normal case, and the case for every vector shipped today — HotSpot remains
the oracle exactly as before.

File format: the extracted lines only, i.e. every line the class printed that
begins with "PASS " or "CK ", in order, one per line. Trailing newlines are
ignored. Do not include stack traces, VM warnings or ANSI colour.

Deliberately EMPTY today
------------------------
The one mode-divergent vector in the corpus, RJdkStrict, is handled by
scheduling rather than by a golden: it asserts the strict outcome (which equals
HotSpot's), and run.sh simply does not schedule it unless --jdk-only is active.
Its compatible-mode behaviour is "allowed to fabricate", which is not a fixed
string anyone should freeze into a file.

Do NOT add a golden here to make a failing diff go away. A golden is for a
vector whose two modes are both correct-but-different, never for recording a
bug as expected. See regression-suite/jdk-only-coverage.txt.
