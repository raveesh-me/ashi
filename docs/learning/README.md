# TigerBeetle concepts: learning track for ashi

Goal: understand every concept that makes TigerBeetle great before ashi implements anything.

Sources: `../tigerbeetle/docs/ARCHITECTURE.md`, `../tigerbeetle/docs/concepts/`, `../tigerbeetle/docs/internals/`, `../tigerbeetle/docs/TIGER_STYLE.md`.

> **Provenance and attribution.**
>
> - These lessons study [TigerBeetle](https://github.com/tigerbeetle/tigerbeetle), © TigerBeetle, Inc., licensed under Apache-2.0.
> - Short code excerpts and quotes are reproduced for commentary and education, under that license.
> - Every `path:line` reference points at TigerBeetle commit `a2a3c706` (2026-11-10).
> - The `../tigerbeetle/...` paths are relative to the ashi workspace, where TigerBeetle is cloned next to this repo as a read-only reference.
> - ashi is not affiliated with TigerBeetle.

## How each drill works

1. **The problem.** What hurts.
2. **The obvious solution, and why it fails.**
3. **What TigerBeetle does,** with pointers into its docs and source.
4. **What it means for ashi in Rust.**
5. **Drill questions.** You answer; we move on only when it sticks. Earlier concepts get re-quizzed at random.

For every concept, ashi takes one stance:
- **Adopt:** do what TigerBeetle does.
- **Adapt:** do something similar, changed for ashi.
- **Differ:** do something else.

These stances become ashi's design doc.

## Curriculum

| # | Concept | Module | Status | ashi stance |
|---|---|---|---|---|
| 1 | [Online transaction processing (OLTP): contention, Amdahl's law, why sharding fails](01-oltp-contention.md) | M0 The problem | 🟡 in progress | — |
| 2 | [Debit/credit as the schema, and its rules](02-debit-credit.md) | M0 | ⬜ | — |
| 3 | [Transactions run inside the database, not over network round trips](03-non-interactive-transactions.md) | M0 | ⬜ | — |
| 4 | [Idempotency keys that follow a transfer end to end; gateways that batch requests](04-end-to-end-idempotency.md) | M0 | ⬜ | — |
| 5 | [The in-memory hash table as the speed limit](05-hash-table-speed-of-light.md) | M1 Single-node speed | ⬜ | — |
| 6 | [Why a single thread wins (COST)](06-single-thread.md) | M1 | ⬜ | — |
| 7 | [Batching](07-batching.md) | M1 | ⬜ | — |
| 8 | [Control logic kept out of the data loop](08-control-vs-data-plane.md) | M1 | ⬜ | — |
| 9 | [Mechanical sympathy: cache lines, 128-byte transfer, struct-of-arrays](09-mechanical-sympathy.md) | M1 | ⬜ | — |
| 10 | [Prefetch data first, then commit without waiting on disk](10-prefetch-and-commit.md) | M1 | ⬜ | — |
| 11 | [Static allocation, so everything has a limit](11-static-allocation.md) | M2 Memory & execution | ⬜ | — |
| 12 | [Explicit state machines instead of async futures; io_uring; intrusive lists](12-explicit-state-machines-io-uring.md) | M2 | ⬜ | — |
| 13 | [Zero dependencies](13-no-dependencies.md) | M2 | ⬜ | — |
| 14 | [How disks fail; why fsync errors can't be trusted; Direct IO](14-storage-fault-model-direct-io.md) | M3 Storage | ⬜ | — |
| 15 | [Hash-chaining and checksums stored outside the data](15-hash-chaining.md) | M3 | ⬜ | — |
| 16 | [Data file layout: 4 superblock copies, the write-ahead log ring, the block grid](16-data-file-layout.md) | M3 | ⬜ | — |
| 17 | [Copy-on-write block grid and checkpoints](17-grid-and-checkpoints.md) | M3 | ⬜ | — |
| 18 | [LSM forest: compaction pacing, manifest, snapshots, index trees](18-lsm-forest.md) | M3 | ⬜ | — |
| 19 | [Timestamps as primary keys](19-timestamps-as-keys.md) | M3 | ⬜ | — |
| 20 | [Replicated state machine; identical logical state *and* identical bytes on disk](20-replicated-state-machine-determinism.md) | M4 Consensus | ⬜ | — |
| 21 | [Viewstamped Replication](21-viewstamped-replication.md) | M4 | ⬜ | — |
| 22 | [Flexible quorums](22-flexible-quorums.md) | M4 | ⬜ | — |
| 23 | [Replication topology](23-replication-topology.md) | M4 | ⬜ | — |
| 24 | [Recovery that tells crashes apart from corruption; NACKs](24-protocol-aware-recovery.md) | M4 | ⬜ | — |
| 25 | [Repairing the write-ahead log and the grid; state sync](25-repair-and-state-sync.md) | M4 | ⬜ | — |
| 26 | [Cluster clock (Marzullo's algorithm)](26-cluster-time.md) | M4 | ⬜ | — |
| 27 | [Client sessions; at-most-once processing](27-client-sessions.md) | M4 | ⬜ | — |
| 28 | [Assuming the network can drop, duplicate or reorder messages](28-message-passing-contract.md) | M4 | ⬜ | — |
| 29 | [VOPR, the deterministic simulator](29-vopr-simulation-testing.md) | M5 Correctness | ⬜ | — |
| 30 | [TigerStyle: assertions, bounds](30-tigerstyle.md) | M5 | ⬜ | — |
| 31 | [Fuzzing each component](31-component-fuzzing.md) | M5 | ⬜ | — |
| 32 | [Upgrades with multiversion binaries](32-multiversion-upgrades.md) | M6 Operations | ⬜ | — |
| 33 | [Strict serializability and what ACID means here](33-strict-serializability-acid.md) | M6 | ⬜ | — |

Legend: ⬜ not started · 🟡 in progress · ✅ mastered

## Where TigerBeetle's docs and code disagree

These were found while writing the lessons, against the clone at `a2a3c706`. The code wins, and each lesson flags its items with ⚠️. Use this list to calibrate how far to trust `docs/`.

| Topic | Docs say | Code does | Lesson |
|---|---|---|---|
| Replication topology | The primary sends to 2 backups, which forward (`ARCHITECTURE.md` Mechanical Sympathy, `vsr.md` Normal) | **Star:** the primary sends to every replica (`replica.zig:1997`). Ring was replaced by adaptive routing in `132d05194` (2025-03), which was replaced by star in `3aa232f95` (2026-04) | [23](23-replication-topology.md), [09](09-mechanical-sympathy.md) |
| Checkpoint interval | 1024 prepares | `vsr_checkpoint_ops` = **960**. 1024 is the WAL ring size | [07](07-batching.md), [17](17-grid-and-checkpoints.md) |
| Batch size | 8,190 transfers per request | **8,189**: the multi-batch trailer takes one 128-byte slot | [02](02-debit-credit.md), [07](07-batching.md) |
| Success result | `ok` | **`created`** (`maxInt(u32)`). `ok` survives only as `deprecated_ok = 0` | [04](04-end-to-end-idempotency.md) |
| Transfer layout | "cache-line aligned" | 128 bytes with **`@alignOf == 16`**. Cache-line placement comes from the message buffers | [09](09-mechanical-sympathy.md) |
| Kernel requirement | Linux ≥ 5.6 | The server needs **≥ 5.11** (`IORING_FEAT_EXT_ARG`); clients use epoll | [12](12-explicit-state-machines-io-uring.md) |
| "io_uring exclusively" | All IO | Linux server only. There are also epoll, kqueue and IOCP backends | [12](12-explicit-state-machines-io-uring.md) |
| "Zero dependencies" | None | Release builds fetch a pinned `llvm-objcopy`; AEGIS is vendored from Zig std | [13](13-no-dependencies.md) |
| Static allocation | Process-wide | Enforced only by the replica's `StaticAllocator`. The message pool is fixed by convention | [11](11-static-allocation.md) |
| "Single-threaded" | The whole system | The replica event loop. `main.zig` uses a threaded IO helper for blocking file operations | [06](06-single-thread.md) |
| Compaction trigger | When a level *exceeds* its limit | When it *reaches* the limit, with one table of overflow allowed (`manifest.zig:543`) | [18](18-lsm-forest.md) |
| `data_file.md` | Free set and sessions in the superblock; several-MB writes | Two compressed bitsets in grid trailers; an 8 KiB header per copy; it omits the `client_replies` and `grid_padding` zones | [16](16-data-file-layout.md), [17](17-grid-and-checkpoints.md) |
| Closed accounts | Pending transfers can't be posted or voided | Voiding is allowed; expiry also reopens the account | [02](02-debit-credit.md) |
| Verification in release builds | Always on (`safety.md`) | Asserts stay on (ReleaseSafe); `constants.verify` checks are **off** | [33](33-strict-serializability-acid.md) |
| Upgrades | Can jump across releases | Each previous release is a must-visit step, so upgrades go one release at a time | [32](32-multiversion-upgrades.md) |
| Client session eviction | The oldest session | The session whose latest committed request is oldest | [27](27-client-sessions.md) |
| TigerStyle 70-line limit | A hard limit | Enforced as a ratchet: about 155 functions are longer | [30](30-tigerstyle.md) |

### Gaps in the simulator's fault model (a chance for ashi to differ)

The docs claim TigerBeetle tolerates these faults, but the whole-cluster simulator (the VOPR) never injects them:

- **Clock skew:** every simulated replica clock has zero drift (`testing/cluster.zig`). Skew is tested only in `vsr/clock.zig`'s unit tests. See [26](26-cluster-time.md) and [29](29-vopr-simulation-testing.md).
- **Network corruption:** the packet simulator drops, delays, duplicates, reorders and partitions packets, but never corrupts them. Corruption is covered only by `message_bus_fuzz.zig`. See [28](28-message-passing-contract.md).
- **Superblock read, write and misdirect faults:** the simulator injects only torn writes into the superblock. This is deliberate, because injecting the other faults could lose the quorum of superblock copies. See [14](14-storage-fault-model-direct-io.md).
- **No continuous fuzzing fleet in the repo:** the CFO was removed in `34a6d6bde` (2026-08). See [31](31-component-fuzzing.md).
