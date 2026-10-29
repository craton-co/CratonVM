# Cold-start probes

These are the programs and scripts behind the "Cold start" section of
[BENCHMARK.md](../../../BENCHMARK.md). The scripts assume this directory is copied to
`/root/p5kit/probes`, with the classes compiled into `cls/`.

- `*.java` — seven short programs that exercise the JDK's start-up paths.
- `cmp.sh <cratonvm> <outdir>` — compares each program's stdout and exit code with
  HotSpot 25, in both CratonVM modes.
- `istat.sh <bin> <mode> <class> [n]` — median `instructions:u` and task-clock over n runs.
- `cmp-img.sh <cratonvm> <outdir>` — compares cold, capture and restored runs with HotSpot, and
  checks that the image was actually used. Set `CAPX=,unsafe-ignore-pointers` to capture past the
  pointer check (diagnosis only).
- `cold.sh <outdir> <rounds> <class...>` — the interleaved bench. It reads
  configurations from `/root/p5kit/cold.conf`; the template is `cold.conf`.

The `*-restored` rows of `cold.conf` expect an image per program and mode under
`/root/p5kit/img/<Class>-<mode>`. A first run with `CRATONVM_BOOT_IMAGE=auto` (plus
`,unsafe-ignore-pointers` until the pointer check passes) captures it.
