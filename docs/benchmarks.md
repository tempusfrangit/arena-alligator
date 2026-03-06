# Benchmarks

This page captures benchmark summaries for local and real-hardware runs.

## Environment

- Host: Apple M4 Max (laptop development baseline)
- Runner: `mise run bench`
- Backend: Plotters (`Gnuplot not found`)

## Summary Tables

Values below use Criterion's reported estimate interval (`low .. high`) and throughput interval from the same run.

### Roundtrip

| Benchmark | Case | Time | Throughput |
| ---- | ---- | ---- | ---- |
| `fixed_roundtrip` | `256` | `67.486 .. 69.471 ns` | `3.4319 .. 3.5329 GiB/s` |
| `fixed_roundtrip` | `1024` | `87.196 .. 89.185 ns` | `10.693 .. 10.937 GiB/s` |
| `fixed_roundtrip` | `4096` | `146.23 .. 149.35 ns` | `25.541 .. 26.086 GiB/s` |
| `buddy_roundtrip` | `256` | `4.5004 .. 4.5067 us` | `54.172 .. 54.248 MiB/s` |
| `buddy_roundtrip` | `1024` | `1.1084 .. 1.1171 us` | `874.16 .. 881.09 MiB/s` |
| `buddy_roundtrip` | `4096` | `341.10 .. 348.22 ns` | `10.955 .. 11.183 GiB/s` |
| `buddy_roundtrip` | `16384` | `297.96 .. 306.89 ns` | `49.720 .. 51.212 GiB/s` |

### Contention

| Benchmark | Case | Time | Throughput |
| ---- | ---- | ---- | ---- |
| `fixed_contention` | `slots_65536_hold_0pct/1` | `93.763 .. 95.334 ns` | `10.489 .. 10.665 Melem/s` |
| `fixed_contention` | `slots_65536_hold_0pct/4` | `118.41 .. 121.76 ns` | `32.851 .. 33.781 Melem/s` |
| `fixed_contention` | `slots_65536_hold_0pct/16` | `260.57 .. 261.14 ns` | `61.270 .. 61.404 Melem/s` |
| `fixed_contention` | `slots_65536_hold_75pct/1` | `88.495 .. 90.919 ns` | `10.999 .. 11.300 Melem/s` |
| `fixed_contention` | `slots_65536_hold_75pct/4` | `118.61 .. 123.17 ns` | `32.475 .. 33.723 Melem/s` |
| `fixed_contention` | `slots_65536_hold_75pct/16` | `260.75 .. 261.52 ns` | `61.181 .. 61.361 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_0pct/1` | `1.3004 .. 1.3058 us` | `765.82 .. 768.98 Kelem/s` |
| `buddy_contention` | `arena_64mb_hold_0pct/4` | `555.60 .. 559.63 ns` | `7.1475 .. 7.1995 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_0pct/16` | `614.72 .. 617.18 ns` | `25.924 .. 26.028 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_50pct/1` | `1.2521 .. 1.2550 us` | `796.79 .. 798.64 Kelem/s` |
| `buddy_contention` | `arena_64mb_hold_50pct/4` | `581.41 .. 583.87 ns` | `6.8509 .. 6.8798 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_50pct/16` | `595.71 .. 598.61 ns` | `26.728 .. 26.859 Melem/s` |

### Auto-spill

| Benchmark | Case | Time | Throughput |
| ---- | ---- | ---- | ---- |
| `fixed_auto_spill` | `2048` | `120.51 .. 123.95 ns` | `15.387 .. 15.827 GiB/s` |
| `fixed_auto_spill` | `8192` | `277.64 .. 279.60 ns` | `27.287 .. 27.479 GiB/s` |

## Regenerating Reports

Generate fresh Criterion reports locally:

```sh
mise run bench
open target/criterion/report/index.html
```

## Real-Iron Benchmarks (k8s)

Source artifact: local unpacked output from a run on a K8S container with 40vCPU (A100-GPU class machine):

- Repeats: `3`
- Modes: `bench` and `bench:extreme`
- Extreme threads: `40`

The tables below are median throughput across the 3 repeats using the mid estimate from each Criterion run.

### k8s Roundtrip + Auto-spill

| Benchmark | Case | Bench | Bench+Extreme |
| ---- | ---- | ---- | ---- |
| `fixed_roundtrip` | `256` | `1.623 GiB/s` | `1.643 GiB/s` |
| `fixed_roundtrip` | `1024` | `5.446 GiB/s` | `5.393 GiB/s` |
| `fixed_roundtrip` | `4096` | `12.981 GiB/s` | `12.469 GiB/s` |
| `buddy_roundtrip` | `256` | `40.814 MiB/s` | `40.886 MiB/s` |
| `buddy_roundtrip` | `1024` | `498.000 MiB/s` | `496.110 MiB/s` |
| `buddy_roundtrip` | `4096` | `4.103 GiB/s` | `4.076 GiB/s` |
| `buddy_roundtrip` | `16384` | `22.099 GiB/s` | `22.158 GiB/s` |
| `fixed_auto_spill` | `2048` | `8.343 GiB/s` | `8.443 GiB/s` |
| `fixed_auto_spill` | `8192` | `18.438 GiB/s` | `18.048 GiB/s` |

### k8s Contention

| Benchmark | Case | Bench | Bench+Extreme |
| ---- | ---- | ---- | ---- |
| `fixed_contention` | `slots_65536_hold_0pct/1` | `5.456 Melem/s` | `5.468 Melem/s` |
| `fixed_contention` | `slots_65536_hold_0pct/4` | `15.455 Melem/s` | `15.149 Melem/s` |
| `fixed_contention` | `slots_65536_hold_0pct/40` | `277.340 Melem/s` | `273.130 Melem/s` |
| `fixed_contention` | `slots_65536_hold_75pct/1` | `5.460 Melem/s` | `5.474 Melem/s` |
| `fixed_contention` | `slots_65536_hold_75pct/4` | `15.576 Melem/s` | `15.388 Melem/s` |
| `fixed_contention` | `slots_65536_hold_75pct/40` | `280.600 Melem/s` | `273.400 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_0pct/1` | `501.630 Kelem/s` | `502.040 Kelem/s` |
| `buddy_contention` | `arena_64mb_hold_0pct/4` | `4.611 Melem/s` | `4.677 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_0pct/40` | `122.940 Melem/s` | `106.670 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_50pct/1` | `511.930 Kelem/s` | `512.200 Kelem/s` |
| `buddy_contention` | `arena_64mb_hold_50pct/4` | `4.145 Melem/s` | `4.673 Melem/s` |
| `buddy_contention` | `arena_64mb_hold_50pct/40` | `122.920 Melem/s` | `137.000 Melem/s` |

