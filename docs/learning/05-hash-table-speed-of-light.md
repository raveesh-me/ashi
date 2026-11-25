# Concept 5: The in-memory hash table as the speed limit

**Module:** M1 Single-node speed · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Intentionality in the Design", "As Fast as a Hash Table", "Batching", "Synchronous Execution", "Embracing Concurrency" and "Mechanical Sympathy"
- `../tigerbeetle/docs/concepts/performance.md`, sections "Batching, Batching, Batching" and "Extreme Engineering"
- `../tigerbeetle/src/state_machine.zig` (`create_transfer`, `account_event`, `prefetch_create_transfers`, `get_account`, `AccountEvent`)
- `../tigerbeetle/src/lsm/groove.zig` (`get`, `prefetch`, `PrefetchContext`)
- `../tigerbeetle/src/lsm/cache_map.zig`, `../tigerbeetle/src/lsm/set_associative_cache.zig`
- `../tigerbeetle/src/vsr/replica.zig` (`CommitStage`)
- `../tigerbeetle/src/config.zig`, `../tigerbeetle/src/constants.zig`

## 1. The problem

How fast *should* a ledger be? Without a target you can't tell whether a design is good or merely familiar. A system doing 10,000 transfers per second might be excellent, or 100× slower than it needs to be. Profiling an existing design only tells you where *that* design spends time. It can't tell you the design is wrong.

You need a **lower bound on the work**, a "speed of light" for the problem. Then every cost you add on top (durability, replication, larger-than-RAM data) has to justify itself against that bound.

## 2. The obvious solution, and why it fails

**Obvious approach 1: start from a general-purpose database and tune.** Add indexes, tune the connection pool, batch some inserts, profile, repeat. Each step is a local improvement: 20% here, 30% there. But the baseline is around ten SQL queries per business transaction, several round trips and row locks (Concepts 1 and 3). Tuning never questions those. `ARCHITECTURE.md` names the anti-pattern: much software "is designed primarily via empirical feedback loop", whereas TigerBeetle thinks "from the first principles what the right solution _should_ be" and then uses experiments to confirm the model ("Intentionality in the Design").

**Obvious approach 2: just use an in-memory hash map.** Fast, but:
- a reboot loses everything (no durability),
- one machine failing takes the ledger down (no availability),
- the data must fit in RAM (no capacity).

The interesting question is how much of the hash map's speed you can keep while fixing all three.

## 3. What TigerBeetle does

### 3.1 The speed-of-light model

`ARCHITECTURE.md`, "As Fast as a Hash Table":

> "a good way to solve financial transactions is an in-memory hash map that stores accounts keyed by their IDs. Processing a transfer is then two hash-map lookups, a balance check, and two balance updates. This is the 'speed-of-light' for the problem --- any other solution wouldn't be significantly faster."

```
accounts: HashMap<u128, Account>

transfer(t):
    dr = accounts[t.debit_account_id]          # lookup 1
    cr = accounts[t.credit_account_id]         # lookup 2
    if dr.debits + t.amount > dr.credits: fail # check
    dr.debits  += t.amount                     # update 1
    cr.credits += t.amount                     # update 2
```

### 3.2 What the real `create_transfer` does

The model is deliberately minimal. Reading `create_transfer` (`src/state_machine.zig:3693`) shows the real per-transfer work:

```
lookups (all served from memory after prefetch):
  1. transfers[t.id]                 idempotency check (Concept 4)
  2. accounts[t.debit_account_id]
  3. accounts[t.credit_account_id]
  (+ transfers[t.pending_id] and transfers_pending[p.timestamp] for post/void)

checks: ~20 cheap branches (ids, flags, ledger, closed, overflow, limits)

writes:
  1. insert Transfer                 128 B
  2. update debit Account            128 B
  3. update credit Account           128 B
  4. insert AccountEvent             256 B  (always, "For CDC", state_machine.zig:4412)
  (+ insert TransferPending 16 B for pending transfers)
```

So the true floor is **about 3 lookups and 4 writes, roughly 640 bytes written per transfer**. That is still a handful of memory operations, but the extra idempotency lookup matters: the transfer-id index is the one that grows forever. `AccountEvent` is asserted to be exactly 256 bytes (`state_machine.zig:218`).

> Not a contradiction, but note: the docs' "two lookups … two updates" leaves out the idempotency lookup and the inserts. Use the fuller list when budgeting.

### 3.3 Back-of-envelope: the floor in nanoseconds

These are textbook orders of magnitude, **not TigerBeetle measurements**. Measure on your own hardware.

| Operation | Rough cost |
|---|---|
| L1/L2 cache hit | ~1–5 ns |
| DRAM access that misses all caches | ~100 ns |
| NVMe random read (4 KiB) | ~10–100 µs |
| Same-datacenter network round trip | ~100–500 µs |
| Durable write: WAL flush + quorum ack | ~0.1–1 ms |

- **Worst-case in-memory floor:** 3 lookups that each miss to DRAM is about 300 ns per transfer, or about 3.3M transfers/s on one core.
- **Hot accounts** (Pareto workloads, Concept 1) sit in cache, so the floor drops further.
- **TigerBeetle's target** of 1M transfers/s per core is a **1 µs budget per transfer**. That is a few times the in-memory floor. The gap is all that durability, replication and capacity may cost, *on average*.

### 3.4 What each requirement costs, and per what unit

| Requirement | Physical cost | If paid per transfer | If paid per batch of 8,189 |
|---|---|---|---|
| **Persistence**: write the request to the write-ahead log (WAL) before replying (Concepts 14 and 16) | one durable write of up to 1 MiB (`message_size_max`, `config.zig:159`) | ~0.5 ms → ~2,000 TPS | ~0.5 ms / 8,189 ≈ **61 ns** |
| **Replication**: a quorum of `quorum_replication_max = 3` replicas must ack (`config.zig:157`; Concepts 21 and 22) | one network round trip | ~0.3 ms → ~3,300 TPS | ≈ **37 ns** |
| **Larger than RAM**: an account isn't in memory | an NVMe read, ~50 µs | catastrophic inside the loop: 1% misses × 2 lookups × 50 µs = **1 µs**, the whole budget | done *before* execution, in parallel, overlapped |
| **Durable LSM state** (Concepts 17 and 18) | compaction writes, checkpoints | — | amortised over 32 prepares (memtable) and 960 prepares (checkpoint) |

The lesson in the table: **persistence and replication are latency costs that batching turns into tiny throughput costs.** Larger-than-RAM data is a different kind of cost. If it shows up inside the execute loop, nothing can amortise it. It has to be moved *out* of the loop.

### 3.5 How TigerBeetle claws back near-hash-table speed

**(a) Batching: pay fixed costs once per thousands of transfers (preview of Concept 7).**
- Up to 8,189 transfers per request (Concept 4). One prepare means one WAL write and one replication round.
- `lsm_compaction_ops = 32` (`config.zig:164`): "Changes from 32 prepares are aggregated in memory before being written to disk together" (`ARCHITECTURE.md`, "Batching"). The `constants.zig:647` comment works it through: 32 × 8,190 = 262,080 transfers per in-memory table.
- Checkpoints every `vsr_checkpoint_ops` prepares (`constants.zig:40`):

  ```zig
  pub const vsr_checkpoint_ops = journal_slot_count -
      lsm_compaction_ops -
      lsm_compaction_ops * stdx.div_ceil(pipeline_prepare_queue_max * 2, lsm_compaction_ops);
  ```

  With the defaults `journal_slot_count = 1024`, `lsm_compaction_ops = 32` and `pipeline_prepare_queue_max = 8` (`config.zig:155–164`): `1024 − 32 − 32 × ceil(16/32) = 960`.

> ⚠️ **Docs vs code:** `ARCHITECTURE.md` ("Batching") says "Changes from 1024 prepares are aggregated before the checkpoint is atomically advanced". 1024 is the WAL ring size (`journal_slot_count`). The code checkpoints every **960** prepares under default config. The margin keeps the next checkpoint's entries from overwriting log entries a quorum still needs (see the long comment at `constants.zig:44`).

**(b) Prefetch, then synchronous execute (preview of Concept 10).** The replica commits each prepare in stages (`src/vsr/replica.zig:83`, `CommitStage`):

```
… → prefetch ("Load required data from LSM tree on disk into memory")
  → execute  ("Execute state machine logic")
  → compact  ("Run one beat of LSM compaction")
  → … checkpoint every 960 ops
```

- **Prefetch.** Given the batch, TigerBeetle knows which keys execution will need *without executing anything*. `prefetch_create_transfers` (`state_machine.zig:1381`) enqueues every `t.id` (and `pending_id`), then the debit/credit account ids, then the pending-status rows. `Groove.prefetch` (`groove.zig:1304`) runs up to `Grid.read_iops_max` lookups in parallel, which is `grid_iops_read_max = 32` (`config.zig:130`).
- **Execute.** `get_account` (`state_machine.zig:4469`) calls `Groove.get` (`groove.zig:840`), which is a pure in-memory lookup: `objects_cache.get_or_tombstone(key)`. No IO and no callbacks. In builds with verification enabled, execution even asserts that each key it touches was enqueued for prefetch ("Ensure that this key was enqueued for prefetching").
- **The hash table, literally.** The object cache is a `CacheMap` (`src/lsm/cache_map.zig`): a set-associative cache (16 ways, 8-bit tags, CLOCK eviction, 64-byte cache-line layout; `set_associative_cache.zig:15`) on top of a "stash" hash map. The stash guarantees that "prefetched values are available in memory during their respective commit". Keys are hashed with `stdx.hash_inline`, a port of Abseil's `low_level_hash` (`stdx.zig:579`).
- **Default sizes.** The production account cache is `@sizeOf(Account) * MiB` = 128 MiB, room for 1,048,576 accounts (`config.zig:222`). The grid block cache defaults to 1 GiB (`config.zig:132`). Both can be overridden at startup.

**(c) Pipelining: overlap the latency costs.** Execution must be sequential, but replication doesn't have to be. The primary "doesn't wait for the replication to finish before starting to work on the next request" (`ARCHITECTURE.md`, "Embracing Concurrency"). Up to `pipeline_prepare_queue_max = 8` prepares are in flight (`config.zig:155`). The WAL write and the replication round trip overlap with execution of earlier prepares.

**(d) Larger-than-RAM without slow lookups: the LSM forest (preview of Concept 18).** "To keep optimal performance, the hot working subset of data should still fit within RAM" (`ARCHITECTURE.md`). LSM trees keep recent data in upper levels, and time-ordered ids and timestamps (Concepts 4 and 19) make "recent" and "hot" mostly the same thing. Compaction is spread evenly in beats, so its cost doesn't land in one prepare.

**(e) Keep the core busy with useful work (Concepts 6 and 9).** One thread, so no lock or cache-line ping-pong. 128-byte objects are exactly two cache lines. Homogeneous batches keep the per-event branch out of the inner loop.

```
           per-transfer cost budget (~1 µs at 1M TPS)
 ┌──────────────────────────────────────────────────────────┐
 │ execute: 3 in-memory lookups + checks + 4 writes  ~ x00ns│ ← the hash-table floor
 │ WAL + replication, amortised over 8,189           ~ <100ns│ ← batching
 │ prefetch IO, parallel & overlapped                ~ hidden│ ← prefetch + pipelining
 │ LSM flush/compaction, amortised over 32 prepares  ~ small │ ← LSM forest
 └──────────────────────────────────────────────────────────┘
```

### 3.6 Speed of light as a design tool

The method, as TigerBeetle applies it:

1. **Write the minimal in-memory algorithm** for the operation, ignoring durability and distribution.
2. **Count unavoidable memory touches** and turn them into a ns/op floor.
3. **List every extra requirement** with its physical unit cost and *what it is paid per*: per op, per batch, per checkpoint, or per cache miss.
4. **Re-shape the design** so the per-op cost stays close to the floor. Move each extra to a coarser unit (batch, checkpoint) or off the critical path (prefetch, pipelining).
5. **Measure against the floor.** If the result is 10× off, look for a missing amortisation in the design before tuning code.

The order matters: the interface (batches, non-interactive transfers, Concept 3) was chosen *so that* steps 3–4 are possible. You can't add batching later to an API that processes one transfer per call.

## 4. What it means for ashi

**Build the floor first.** Before any storage code, write ashi's `create_transfer` against a plain in-memory map and benchmark a loop of batches. That number is ashi's speed of light. Every later layer is measured as "ns/transfer added".

**Rust features that help:**
- Monomorphised generics and `#[inline]` give zero-cost abstractions for the cache and groove layers, much as Zig's `comptime` does.
- Slices of `#[repr(C)]` structs (`&[Transfer]`) iterate with predictable, prefetch-friendly access.

**Pitfalls specific to Rust:**
- **`std::collections::HashMap` uses SipHash by default.** It is DoS-resistant and noticeably slower for 16-byte keys. Behind an authenticated gateway, a fast non-cryptographic hasher is reasonable: `foldhash` (hashbrown's default), `ahash`, `rustc-hash`, or a port of `low_level_hash` like TigerBeetle's.
- **Growth and rehashing.** A growing `HashMap` reallocates and rehashes, causing latency spikes and allocation in the hot path. That violates Concept 11. Pre-size with `with_capacity`, or better, build a fixed-capacity set-associative cache like TigerBeetle's.
- **Hidden allocation and indirection.** A `Vec`/`Box`/`String` per transfer, `Arc` clones, or `dyn Trait` calls in the loop. Each costs more than the whole floor budget for a transfer.
- **Benchmarks that lie.** Uniformly random account ids defeat caches. Pareto-distributed ids flatter them. Benchmark both, plus a "cold" run where most accounts miss.
- **Async in the loop.** An `.await` inside execute is a potential IO stall, the exact thing prefetch exists to remove.

**Crates worth evaluating:**
- `hashbrown` (already inside std) and `foldhash`/`ahash` for the baseline map.
- `criterion` or `divan` for microbenchmarks; `perf stat -e cache-misses` or `cargo flamegraph` for where the time goes.
- For production: a hand-rolled static set-associative cache. TigerBeetle has zero dependencies, and this structure is small, central and worth owning (Concept 13).

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt** speed-of-light budgeting as ashi's primary design method. ashi's own measured floor, not TigerBeetle's numbers, sets its per-transfer budget.

## 5. Drill questions

### Q1. Recall

The docs describe the speed of light as "two hash-map lookups, a balance check, and two balance updates." List what TigerBeetle's real `create_transfer` adds on top, and the size of each record it writes.

> _Your answer:_

### Q2. Mechanism

Explain why TigerBeetle's execute step never reads from storage. What does prefetch know before execution, why can prefetch run in parallel while execute must be sequential, and what guarantees a prefetched value is still in memory when execute needs it?

> _Your answer:_

### Q3. Back-of-envelope

Assume: DRAM miss 100 ns, NVMe read 50 µs, and WAL write + quorum replication 500 µs per prepare (not overlapped).

1. One transfer per prepare: maximum transfers per second?
2. 8,189 transfers per prepare: amortised durability cost per transfer?
3. No prefetch: 2% of account lookups inside execute miss to disk. Average added cost per transfer, and the resulting throughput ceiling from that alone?

> _Your answer:_

### Q4. What breaks if…

…the hot working set grows to 5 million accounts while the account cache stays at its 128 MiB default? What gets slower, what stays correct, and which knob would you turn first?

> _Your answer:_

### Q5. ashi design (open-ended)

Design ashi's speed-of-light experiment. What exactly do you benchmark (data structure, key distribution, batch size, record layout)? What do you count as "the floor"? Which three overheads do you add first, in what order, and what ns/transfer budget do you give each?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- **Extra lookup:** the idempotency check `transfers[t.id]`. For post/void, also `transfers[t.pending_id]` and the `transfers_pending` status row.
- **Writes:**
  - insert the `Transfer` (128 B),
  - update the two `Account`s (128 B each),
  - insert an `AccountEvent` (256 B), always, for change data capture and history,
  - for pending transfers, a 16-byte `TransferPending`.
- **Checks:** about 20 cheap validation branches, including overflow checks.
- **Total:** roughly 640 bytes written per transfer, plus index entries in the LSM.

**Q2.**
- Execution is sequential, on one thread. One disk read inside it (~50 µs) stalls every transfer behind it.
- From the batch alone, prefetch knows every key execution can touch: transfer ids, pending ids, debit/credit account ids. It doesn't need to run any logic to know them. The lookups are independent, so they can be issued concurrently (up to `grid_iops_read_max = 32` reads).
- Execution must be sequential because each transfer can change balances the next one checks.
- Prefetched values are held by the `CacheMap`'s stash, which guarantees they stay in memory for that commit even if the set-associative part evicts them.
- With verification enabled, `create_transfer` asserts that each key was enqueued for prefetch.

**Q3.**
1. 500 µs per transfer → **2,000 TPS**.
2. 500 µs / 8,189 ≈ **61 ns** per transfer.
3. 2 lookups × 2% × 50 µs = **2 µs** per transfer on average. That caps throughput at about **500k TPS**, half the 1M target, from this alone. It also makes latency spiky. This is why prefetch exists.

**Q4.**
- 128 MiB / 128 B = 1,048,576 cached accounts, so most of a 5M hot set doesn't fit.
- **Correctness is unaffected.** Execution still only reads prefetched values, and the stash guarantees they are present.
- **What gets slower:** prefetch, because more lookups go to the LSM and disk. Per-batch latency grows, and throughput drops if prefetch IO can no longer hide behind pipelined replication. The grid block cache (default 1 GiB) absorbs some of the reads.
- **First knob:** raise the account cache (`--cache-accounts`, or `--memory` with its default split), sized to the hot set: 5M × 128 B ≈ 640 MiB plus headroom. Then check whether id locality (time-based ids) keeps hot keys in upper LSM levels.

**Q5.** Open-ended. A strong answer:
- **Benchmark:** `create_transfer` over `&[Transfer]` batches of 8,189, against a pre-sized open-addressing map or a set-associative cache. Use 128-byte `#[repr(C, align(16))]` records and run three key distributions: uniform, Zipf/Pareto and cold.
- **The floor:** ns/transfer for execute-only with everything in memory, including the idempotency lookup and all four writes (not just the two-update textbook version).
- **Add overheads in this order:**
  1. the WAL append with real flush, amortised per batch (budget ~50–100 ns),
  2. prefetch from an LSM with a cold cache (budget: hidden behind pipelining, so near 0 on the critical path),
  3. replication to a quorum (budget ~50 ns amortised).
- **Report:** each layer as a delta over the floor. A layer that blows its budget triggers a *design* review (missing amortisation), not micro-tuning.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
