# CratonBench 4-Way CPU Benchmark Results

**Date**: 2026-10-31 14:07:46 UTC  
**Host**: AMD EPYC-Turin (16 vCPUs, 60 GB RAM, Linux x86_64)  
**Configurations**:
1. `CVM Default`: CratonVM dev tip (ZGC default)
2. `CVM Fast-Boot`: CratonVM dev tip (`--fast-boot`)
3. `HotSpot`: OpenJDK 25 (`25.0.4.1`)
4. `GraalVM`: Oracle GraalVM 25 (`25.0.4` LTS)


### Arithmetic

| Iteration | CVM Default | CVM Fast-Boot | HotSpot | GraalVM | CVM Def / HotSpot | CVM Fast / HotSpot | Checksum Match |
|:---|---:|---:|---:|---:|---:|---:|:---:|
| **1B** | 1,253 ms | 1,256 ms | 1,105 ms | 1,079 ms | 1.13x | 1.14x | PASS |
| **1.5B** | 1,883 ms | 1,880 ms | 1,727 ms | 1,614 ms | 1.09x | 1.09x | PASS |
| **2B** | 2,506 ms | 2,507 ms | 2,210 ms | 2,147 ms | 1.13x | 1.13x | PASS |
| **2.4B** | 3,007 ms | 3,009 ms | 2,651 ms | 2,578 ms | 1.13x | 1.14x | PASS |
| **3B** | 3,761 ms | 3,761 ms | 3,313 ms | 3,222 ms | 1.14x | 1.14x | PASS |

### Fibonacci

| Iteration | CVM Default | CVM Fast-Boot | HotSpot | GraalVM | CVM Def / HotSpot | CVM Fast / HotSpot | Checksum Match |
|:---|---:|---:|---:|---:|---:|---:|:---:|
| **40** | 68 ms | 67 ms | 255 ms | 180 ms | 0.27x | 0.26x | PASS |
| **42** | 175 ms | 175 ms | 665 ms | 464 ms | 0.26x | 0.26x | PASS |
| **44** | 456 ms | 456 ms | 1,746 ms | 1,215 ms | 0.26x | 0.26x | PASS |
| **46** | 1,195 ms | 1,191 ms | 4,562 ms | 3,174 ms | 0.26x | 0.26x | PASS |
| **48** | 3,114 ms | 3,112 ms | 11,935 ms | 8,229 ms | 0.26x | 0.26x | PASS |

### Sieve

| Iteration | CVM Default | CVM Fast-Boot | HotSpot | GraalVM | CVM Def / HotSpot | CVM Fast / HotSpot | Checksum Match |
|:---|---:|---:|---:|---:|---:|---:|:---:|
| **50K × 10K** | 701 ms | 715 ms | 613 ms | 468 ms | 1.14x | 1.17x | PASS |
| **50K × 20K** | 1,409 ms | 1,402 ms | 1,121 ms | 859 ms | 1.26x | 1.25x | PASS |
| **100K × 20K** | 3,476 ms | 3,487 ms | 2,249 ms | 1,612 ms | 1.55x | 1.55x | PASS |
| **100K × 40K** | 6,778 ms | 6,768 ms | 6,619 ms | 3,152 ms | 1.02x | 1.02x | PASS |
| **100K × 60K** | 10,150 ms | 10,210 ms | 9,578 ms | 4,633 ms | 1.06x | 1.07x | PASS |

### Matrix

| Iteration | CVM Default | CVM Fast-Boot | HotSpot | GraalVM | CVM Def / HotSpot | CVM Fast / HotSpot | Checksum Match |
|:---|---:|---:|---:|---:|---:|---:|:---:|
| **720x720** | 462 ms | 460 ms | 454 ms | 470 ms | 1.02x | 1.01x | PASS |
| **960x960** | 1,057 ms | 1,053 ms | 1,063 ms | 1,063 ms | 0.99x | 0.99x | PASS |
| **1280x1280** | 2,614 ms | 2,569 ms | 2,588 ms | 2,586 ms | 1.01x | 0.99x | PASS |
| **1920x1920** | 10,630 ms | 10,456 ms | 12,432 ms | 10,590 ms | 0.86x | 0.84x | PASS |
| **2560x2560** | 66,086 ms | 68,055 ms | 62,574 ms | 61,061 ms | 1.06x | 1.09x | PASS |

### Binary Trees

| Iteration | CVM Default | CVM Fast-Boot | HotSpot | GraalVM | CVM Def / HotSpot | CVM Fast / HotSpot | Checksum Match |
|:---|---:|---:|---:|---:|---:|---:|:---:|
| **16** | 322 ms | 324 ms | 140 ms | 132 ms | 2.30x | 2.31x | PASS |
| **18** | 1,453 ms | 1,487 ms | 450 ms | 434 ms | 3.23x | 3.30x | PASS |
| **20** | 6,728 ms | 6,448 ms | 1,504 ms | 1,572 ms | 4.47x | 4.29x | PASS |
| **22** | 31,076 ms | 28,626 ms | 5,951 ms | 6,225 ms | 5.22x | 4.81x | PASS |
| **24** | 141,546 ms | 137,968 ms | 24,632 ms | 26,012 ms | 5.75x | 5.60x | PASS |

### HashMap

| Iteration | CVM Default | CVM Fast-Boot | HotSpot | GraalVM | CVM Def / HotSpot | CVM Fast / HotSpot | Checksum Match |
|:---|---:|---:|---:|---:|---:|---:|:---:|
| **1M** | 79 ms | 78 ms | 81 ms | 86 ms | 0.98x | 0.96x | PASS |
| **5M** | 255 ms | 251 ms | 330 ms | 219 ms | 0.77x | 0.76x | PASS |
| **10M** | 493 ms | 493 ms | 616 ms | 533 ms | 0.80x | 0.80x | PASS |
| **50M** | 3,802 ms | 3,852 ms | 2,987 ms | 2,354 ms | 1.27x | 1.29x | PASS |
| **100M** | 12,557 ms | 12,378 ms | 5,640 ms | 4,914 ms | 2.23x | 2.19x | PASS |

### String / Regex

| Iteration | CVM Default | CVM Fast-Boot | HotSpot | GraalVM | CVM Def / HotSpot | CVM Fast / HotSpot | Checksum Match |
|:---|---:|---:|---:|---:|---:|---:|:---:|
| **100K** | 65 ms | 65 ms | 24 ms | 30 ms | 2.71x | 2.71x | PASS |
| **500K** | 188 ms | 207 ms | 52 ms | 106 ms | 3.62x | 3.98x | PASS |
| **1M** | 352 ms | 340 ms | 78 ms | 83 ms | 4.51x | 4.36x | PASS |
| **5M** | 1,645 ms | 1,602 ms | 290 ms | 247 ms | 5.67x | 5.52x | PASS |
| **10M** | 3,066 ms | 3,233 ms | 563 ms | 458 ms | 5.45x | 5.74x | PASS |