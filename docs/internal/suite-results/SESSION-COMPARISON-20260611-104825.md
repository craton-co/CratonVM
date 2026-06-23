# Cross-VM comparison — session run

- **CratonVM:** `C:/craton/CratonVM-bench/target/release/cratonvm.exe`
- **HotSpot:** `/c/Program Files/Common Files/Oracle/Java/javapath/java`
- **TornadoVM:** `C:/craton/tornadovm/jdk-25.0.3/bin/java.exe` (argfile)
- **Heap:** 8g (bench), per-suite for BC; **timeout:** 360s

## §1 Micro-benchmarks — wall time (ms)

| benchmark  | cratonvm-jit | cratonvm-nojit | hotspot | tornadovm |
|------------|--------------|----------------|---------|-----------|
| arith1500M | 9388         | NA             | 5044    | 5495      |
| fib44      | 7901         | NA             | 4052    | 4272      |
| sieve250k  | 2429         | 142337         | 1652    | 1428      |
| matrix600  | 876          | 58730          | 523     | 497       |
| bintrees18 | 14824        | NA             | 688     | 641       |
| vadd2_28   | 2455         | 197827         | 1805    | 1688      |

## §1 Micro-benchmarks — state

| benchmark  | cratonvm-jit | cratonvm-nojit | hotspot | tornadovm |
|------------|--------------|----------------|---------|-----------|
| arith1500M | OK           | TIMEOUT        | OK      | OK        |
| fib44      | OK           | TIMEOUT        | OK      | OK        |
| sieve250k  | OK           | OK             | OK      | OK        |
| matrix600  | OK           | OK             | OK      | OK        |
| bintrees18 | OK           | TIMEOUT        | OK      | OK        |
| vadd2_28   | OK           | OK             | OK      | OK        |

## §1 Micro-benchmarks — checksum (must match across a row)

| benchmark  | cratonvm-jit        | cratonvm-nojit     | hotspot             | tornadovm           |
|------------|---------------------|--------------------|---------------------|---------------------|
| arith1500M | 2812500002999999995 | NA                 | 2812500002999999995 | 2812500002999999995 |
| fib44      | 701408733           | NA                 | 701408733           | 701408733           |
| sieve250k  | 22044               | 22044              | 22044               | 22044               |
| matrix600  | 6479950792          | 6479950792         | 6479950792          | 6479950792          |
| bintrees18 | 68332206            | NA                 | 68332206            | 68332206            |
| vadd2_28   | 108086390654238720  | 108086390654238720 | 108086390654238720  | 108086390654238720  |

## §1b JUnit Platform --help — state / wall(s)

| item       | cratonvm-jit | cratonvm-nojit | hotspot | tornadovm |
|------------|--------------|----------------|---------|-----------|
| junit-help | OK           | OK             | OK      | OK        |

| item       | cratonvm-jit | cratonvm-nojit | hotspot | tornadovm |
|------------|--------------|----------------|---------|-----------|
| junit-help | 20.8         | 7.0            | 0.8     | 1.2       |

## §2 Bouncy Castle — state

| bc-suite               | cratonvm | hotspot | tornadovm |
|------------------------|----------|---------|-----------|
| asn1-regression        | OK       | OK      | OK        |
| math-ec                | TIMEOUT  | OK      | OK        |
| math-raw               | OK       | OK      | OK        |
| math                   | OK       | OK      | OK        |
| crypto-regression      | TIMEOUT  | OK      | OK        |
| crypto-prng-regression | OK       | OK      | OK        |
| pqc-crypto-regression  | OK       | OK      | OK        |
| util-encoders          | OK       | OK      | OK        |

## §2 Bouncy Castle — wall (s)

| bc-suite               | cratonvm | hotspot | tornadovm |
|------------------------|----------|---------|-----------|
| asn1-regression        | 57.2     | 1.5     | 1.9       |
| math-ec                | 360.2    | 50.1    | 49.4      |
| math-raw               | 2.7      | 0.6     | 1.1       |
| math                   | 9.2      | 1.2     | 1.5       |
| crypto-regression      | 360.2    | 169.2   | 178.5     |
| crypto-prng-regression | 65.7     | 1.0     | 1.3       |
| pqc-crypto-regression  | 234.1    | 2.2     | 2.7       |
| util-encoders          | 36.5     | 0.7     | 1.1       |

## §3 Commons Math full reactor

| variant | rc | wall(s) | tests | fail | skip | build |
|---------|----|---------|-------|------|------|-------|
| hotspot | 0 | 128.7 | 3204 | 0 | 30 | BUILD SUCCESS |
| tornadovm | 0 | 174.6 | 3204 | 0 | 30 | BUILD SUCCESS |
| cratonvm | 1 | 229.1 | 56 | 12 | 30 | RAN(transform-only) |

