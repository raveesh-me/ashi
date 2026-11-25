# Concept 10: Prefetch data first, then commit without waiting on disk

**Module:** M1 Single-node speed · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Synchronous Execution" (lines 424–434), "Embracing Concurrency" (436–465) and "As Fast as a Hash Table" (122–138)
- `../tigerbeetle/docs/internals/lsm.md` (mutable/immutable tables, bars and beats)
- `../tigerbeetle/src/vsr/replica.zig` (`CommitStage`, `commit_dispatch`, `commit_prefetch`, `commit_execute`, `execute_op`)
- `../tigerbeetle/src/state_machine.zig` (`prefetch`, `prefetch_create_transfers*`, `prefetch_finish`, `commit`, `execute_create`, `create_transfer`, `get_account`)
- `../tigerbeetle/src/lsm/groove.zig` (`prefetch_keys`, `objects_cache`, `get`, `prefetch_enqueue`, `prefetch`, `PrefetchContext`, `PrefetchWorker`, `insert`)
- `../tigerbeetle/src/lsm/cache_map.zig`, `../tigerbeetle/src/lsm/set_associative_cache.zig`
- `../tigerbeetle/src/lsm/tree.zig` (`lookup_from_levels_cache`)
- `../tigerbeetle/src/vsr/grid.zig` (block cache), `../tigerbeetle/src/config.zig`, `../tigerbeetle/src/constants.zig`
- Line numbers are from the clone at commit `a2a3c70`.

## 1. The problem

Executing a transfer needs the current state of two accounts. With a dataset larger than RAM (Concept 5), some of those accounts live only on disk, in the LSM tree (Concept 18).

A disk read takes ~10–100 µs on NVMe (a general hardware figure). The CPU work for a transfer is well under 1 µs (Concept 6). If execution ever waits on a read, the core sits idle for 10–100 transfers' worth of time.

Execution must also be **sequential** and **deterministic**. Transfer 5 may depend on the balance transfer 4 just wrote, and every replica must produce identical results (Concept 20).

So the problem is: how do you keep the CPU loop free of IO waits without giving up sequential, deterministic execution?

## 2. The obvious solution, and why it fails

### Option A: read on demand, blocking

```rust
for t in transfers {
    let dr = storage.get_account(t.debit)?;   // may block 100 µs on disk
    let cr = storage.get_account(t.credit)?;
    apply(t, dr, cr);
}
```

Worst case for a batch of 8,189 transfers with cold accounts: `2 × 8,189 × 100 µs ≈ 1.6 s` per batch. The disk can serve dozens of reads at once, but this loop issues one at a time.

### Option B: read on demand, async

```rust
for t in transfers {
    let dr = storage.get_account(t.debit).await;
    ...
}
```

This still serializes the reads, because each `.await` happens before the next transfer starts. Overlapping them means running transfers concurrently, which brings back:

- interleaving;
- the need to order conflicting transfers on the same account;
- nondeterminism across replicas;
- a future (often a heap-allocated `Box<dyn Future>`) per in-flight transfer.

### Option C: a big cache and hope

Put everything in a cache and read synchronously, assuming hits. Any miss stalls the loop, so the tail latency is set by the worst miss.

## 3. What TigerBeetle does

The key insight (`ARCHITECTURE.md:432–434`):

> "Given a batch of transfers, it is possible to predict which accounts need to be fetched without actually executing the transfers. What's more, while commit execution has to happen sequentially, all prefetch IO can happen in parallel."

So execution splits into two phases:

1. **Prefetch** (asynchronous, parallel IO): read every object the batch *might* touch into memory.
2. **Commit** (synchronous, single tight loop): execute the batch against memory only. "Crucially, the `commit` function itself doesn't do any reading from storage. This is the key to performance." (`ARCHITECTURE.md:429–430`)

### 3.1 Where it sits in the replica's commit pipeline

The replica commits one prepare at a time through an explicit state machine (Concept 12). `CommitStage` (`src/vsr/replica.zig:83–118`) lists the stages. `commit_dispatch` (`replica.zig:4398`) walks them in order:

```
 idle → start → check_prepare
          │
          ▼
      prefetch ──────────── async: returns .pending, resumes on callback
          │
          ▼
       stall ─────────────── primary only: backpressure if backups lag
          │
          ▼
    reply_setup
          │
          ▼
      execute ────────────── SYNCHRONOUS: state_machine.commit(), no IO
          │
          ▼
  checkpoint_durable ─────── every checkpoint: wait for grid durability
          │
          ▼
      compact ────────────── one beat of LSM compaction (async)
          │
          ▼
  checkpoint_data / checkpoint_superblock ─── only at checkpoint boundaries
          │
          ▼
        idle  (next prepare)
```

Two facts follow from `commit_dispatch`:

- **`execute` has no pending path.** `commit_execute` (`replica.zig:4917`) returns `void`. Every other stage may return `.pending` and resume later.
- **One prepare at a time.** There is a single `commit_prepare`. The prefetch for op N+1 starts after op N's stages finish.

`commit_prefetch` (`replica.zig:4739–4790`) picks the LSM **snapshot** to read from and starts the state machine's prefetch:

```zig
self.state_machine.prefetch(
    commit_prefetch_callback,
    prepare.header.op,
    snapshot,
    prepare_operation,
    prepare.body_used(),
);
return .pending;
```

When IO finishes, `commit_prefetch_callback` (`replica.zig:4792`) calls `commit_dispatch_resume()`, and the loop continues to `execute`. That calls `execute_op`, which calls `state_machine.commit(...)` (`replica.zig:5460–5467`).

### 3.2 The prefetch phase in `state_machine.zig`

`StateMachine.prefetch` (`src/state_machine.zig:1220–1300`):

1. Asserts that no other prefetch is in progress (`prefetch_operation == null`, …).
2. Decodes the multi-batch body and asserts each batch is valid.
3. Calls `prefetch_begin(snapshot)` on the three grooves. A **groove** is TigerBeetle's object store for one type: an object tree plus its index trees and an object cache.
4. Dispatches on the operation, once (Concept 8).

For `create_transfers`, prefetch runs as a **chain of three groove prefetches**, each with parallel IO inside:

```
prefetch_create_transfers                         state_machine.zig:1381
  for each transfer t:
     transfers.prefetch_enqueue(t.id)              ← idempotency: does t already exist?
     if t posts/voids a pending transfer:
        transfers.prefetch_enqueue(t.pending_id)   ← need the pending transfer
     if t.flags.imported:
        accounts.prefetch_enqueue(timestamp)       ← timestamp-collision check
  transfers.prefetch(...)  ── parallel IO ──┐
                                            ▼
prefetch_create_transfers_callback_transfers      state_machine.zig:1415
  for each transfer t:
     if post/void:  p = get_transfer(t.pending_id)       (now in memory)
                    transfers_pending.enqueue(p.timestamp)
                    accounts.enqueue(p.debit_account_id, p.credit_account_id)
     else:          accounts.enqueue(t.debit_account_id, t.credit_account_id)
  accounts.prefetch(...)  ── parallel IO ──┐
                                           ▼
prefetch_create_transfers_callback_accounts       state_machine.zig:1458
  transfers_pending.prefetch(...)  ── parallel IO ──┐
                                                    ▼
prefetch_create_transfers_callback_transfers_pending   state_machine.zig:1474
  prefetch_finish()  →  replica callback          state_machine.zig:1302
```

**How the account set is predicted from the batch.**
- A normal transfer names both accounts directly, so they are known at once (`state_machine.zig:1447–1448`).
- A **post/void** of a pending transfer does not carry account ids. They live in the pending transfer it refers to. So prefetch first loads the pending transfer, then enqueues its accounts (`state_machine.zig:1431–1445`).

That dependency is why there are two IO rounds instead of one. The prediction depth is **bounded**: at most one hop.

The prediction may **over-fetch**:
- It enqueues accounts for transfers that will later fail validation.
- It enqueues duplicates; `prefetch_enqueue` tolerates them (`groove.zig:955–960`).

That only costs a little IO. **Under-fetching would be a bug**, and the commit phase asserts it never happens (3.4).

### 3.3 Inside a groove: memory first, then parallel disk reads

**`prefetch_enqueue`** (`src/lsm/groove.zig:961`) is synchronous. It resolves as much as possible from memory before scheduling any IO. For each key it records a status in `prefetch_keys`, a hash map pre-sized at startup (`groove.zig:666–670`, `795–800`):

1. **Object cache hit** (by primary key) → `.found`. Done.
2. **Key range check.** `key_range_contains` says the key is outside every table's range → `.not_found`. Done, with no IO. New ids usually land here.
3. **In-memory LSM search** via `lookup_from_levels_cache` (`src/lsm/tree.zig:253`). This checks the immutable table, then index and value blocks *already in the grid block cache*. It returns:
   - `.negative` → `.not_found`;
   - `.positive` → copy into the object cache, `.found`;
   - `.possible(level)` → `.prefetching`, with a level hint where IO must start.

**`prefetch`** (`groove.zig:1304`) then starts the workers for the keys still in `.prefetching`. `PrefetchContext` holds a fixed array of workers (`groove.zig:1326–1329`):

```zig
/// The goal is to fully utilize the disk I/O to ensure the prefetch completes as
/// quickly as possible, so we run multiple lookups in parallel based on the max
/// I/O depth of the Grid.
workers: [Grid.read_iops_max]PrefetchWorker = undefined,
```

- `Grid.read_iops_max` is `constants.grid_iops_read_max` (`src/vsr/grid.zig:38`), 32 by default (`src/config.zig:130`).
- Each worker pulls the next `.prefetching` entry from a shared iterator (`lookup_start_next`, `groove.zig:1509`). It issues an LSM read from storage (`lookup_from_levels_storage`), and in the callback upserts the object into the cache (`groove.zig:1700–1730`).
- When the last worker finishes, `finish` (`groove.zig:1393`) calls back into the state machine.
- If every key resolved from memory, the callback still runs **asynchronously**, on the next tick (`start_workers`, `groove.zig:1335`). Callers never see a mix of synchronous and asynchronous completion.

The timeline on one core:

```
CPU   │enqueue 8,189 ids│ submit ≤32 reads │  idle / other events  │cb│cb│…│ commit loop (pure CPU) │
disk  │                 │■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■│        (no IO)        │
                          ↑ up to 32 lookups in flight, refilled as each completes
```

Prefetch **IO is parallel**; commit **CPU work is sequential**. Neither waits on the other mid-loop.

### 3.4 The commit phase reads only memory

`StateMachine.commit` (`state_machine.zig:2538`) → `execute_multi_batch` (`2676`) → `execute_create` (`2976`) → `create_transfer` (`3693`). Every read goes through the groove's `get`:

```zig
fn get_account(self: *const StateMachine, id: u128) ?Account {      // state_machine.zig:4469
    return switch (self.forest.grooves.accounts.get(id)) {
        .found_object => |object| object,
        .not_found => null,
    };
}
```

`Groove.get` (`groove.zig:839–905`) takes `*const Groove` and consults **only** `objects_cache`. There is no IO path at all. A miss means "does not exist", and that is only valid because prefetch already looked.

TigerBeetle checks this assumption. `create_transfer` asserts that every missing key was actually enqueued and resolved as `.not_found` (`state_machine.zig:3748–3756`):

```zig
const dr_account = self.get_account(t.debit_account_id) orelse {
    // Ensure that this key was enqueued for prefetching.
    if (constants.verify) assert(
        self.forest.grooves.accounts.prefetch_keys.get(.{
            .id = t.debit_account_id,
        }).? == .not_found,
    );
    return .debit_account_not_found;
};
```

If prefetch ever missed a key, the `.?` unwrap of a null map entry would fail and crash under `verify`. Without that check, the transfer would get a wrong `debit_account_not_found`. `Groove.get` carries similar `constants.verify` cross-checks against `prefetch_keys` (`groove.zig:850–902`).

### 3.5 "Without waiting on disk": where writes go

Writes in `create_transfer` are also memory-only:

- `transfers.insert(...)` and `accounts.update(...)` (`state_machine.zig:3892–3945`) put values into each tree's **mutable table** and the object cache (`Groove.insert`, `groove.zig:1736`).
- `docs/internals/lsm.md`: "All tree updates, inserts, and removes are applied to the mutable table", and "The mutable table's size is allocated to accommodate a full bar of updates."

The disk catches up later:
- the `compact` stage runs one **beat** of compaction per op (Concept 7);
- the mutable table becomes immutable at the end of a bar and is flushed to level 0;
- the `checkpoint_*` stages make it all durable every 960 ops.

Why lazy persistence is safe: the prepare itself is already durable. A prepare commits only after a replication quorum has written it to their WAL (`ARCHITECTURE.md:45–48`). After a crash, a replica reloads the last checkpoint and **replays** the WAL suffix. Determinism guarantees it reaches the same state (`ARCHITECTURE.md:72–78`; Concepts 17 and 20).

The docs add that the primary "concurrently executes a prepare while still writing the corresponding message to the (WAL)" (`ARCHITECTURE.md:456–458`). The replication quorum need not include the primary's own write. `on_prepare_ok` counts the primary's own ack toward the quorum when it arrives ("including ourself", `replica.zig:2286`). But the threshold `quorum_replication` can also be met by backups alone.

### 3.6 The caches involved

| Cache | Type | Holds | Default size |
|---|---|---|---|
| Groove **object cache** (`objects_cache`) | `CacheMap` = optional `SetAssociativeCache` + "stash" `HashMap` (`src/lsm/cache_map.zig:11–25`) | Whole objects by primary key (`Account` by id, `Transfer` by id, `TransferPending` by timestamp) | Accounts: `@sizeOf(Account) * MiB` = 128 MiB (`config.zig:222`); transfers and pending transfers: 0 (`config.zig:223–224`). Operators override these on the CLI. |
| **Stash** inside `CacheMap` | `std.HashMapUnmanaged`, capacity fixed at init | Every prefetched or mutated object for the current bar; guaranteed present during commit | `lsm_compaction_ops × (batch_value_count_limit + prefetch_entries_for_read_max)` (`groove.zig:727–737`) |
| **Grid block cache** | `SetAssociativeCache` of block addresses, 16 ways (`src/vsr/grid.zig:159–175`) | 512 KiB index and value blocks | `grid_cache_size_default = GiB` (`config.zig:132`) |
| `prefetch_keys` | Hash map, pre-sized | Status of every key enqueued this prefetch | per groove, sized from batch limits |

`CacheMap`'s doc comment states the split of responsibilities (`cache_map.zig:11–24`):

> "The SetAssociativeCache sits on top and absorbs the majority of get / put requests. Below that, lives a HashMap. Should an insert() cause an eviction ..., the evicted value is caught and put in the stash. This allows for a potentially huge cache, with all the advantages of CLOCK Nth-Chance, while still being able to give hard guarantees that values will be present."

The lookup hierarchy (`cache_map.zig:82`): "cache (if present) -> stash -> immutable table -> lsm."

This is the answer to Option C's weakness. The set-associative cache is **opportunistic**: it may evict. The stash is the **guarantee**: whatever prefetch loaded for this batch cannot be evicted before commit reads it.

Why the default transfer cache is 0: the comment at `constants.zig:150–162` ranks caching priority. Accounts get "2 lookups per created transfer, high temporal locality, positive expected result". Transfers have "low temporal locality, negative expected result": a new transfer id is usually *not* found, and the key-range check answers that without a cache.

## 4. What it means for ashi

### Rust features that help

- **A synchronous commit fits the borrow checker well.** `fn commit(&mut self, batch: &[Transfer], out: &mut [Result])` has no `.await`, no `Send` bounds, no pinned futures and no lifetimes crossing suspension points.
- **Exhaustive `match`** on a per-key status enum (`Enqueued | Prefetching { level } | Found(pk) | NotFound`) mirrors `prefetch_keys` and forces every state to be handled.

### Patterns

- **Split the state-machine trait into two phases:**

```rust
trait StateMachine {
    /// Enumerate every key the batch may read. Pure; no IO.
    fn prefetch_keys(&self, op: Operation, body: &[u8], keys: &mut PrefetchSet);
    /// Execute against memory only. Must never touch storage.
    fn commit(&mut self, op: Operation, ts: u64, body: &[u8], out: &mut [u8]) -> usize;
}
```

- **Make the commit-time store type unable to do IO.** Give `commit` a `&CacheView` that has `get()` but no storage handle. Then "commit never reads disk" is enforced by types, not by convention.
- **Bound the prediction depth.** Every new operation must declare how many prefetch rounds it needs (TigerBeetle: 1 for plain transfers, 2 for post/void). Reject designs with data-dependent chains of unbounded depth.
- **Resolve from memory before scheduling IO** (cache → key range → cached blocks), then run N workers from a shared iterator, with N = the IO depth you budgeted (32 in TigerBeetle).
- **Assert "prefetched" in commit** under a `verify` feature, exactly like `create_transfer` does.

### Crates worth evaluating (TigerBeetle has zero dependencies)

| Crate | Use | Trade-off |
|---|---|---|
| `hashbrown` (std's `HashMap` backend) | Stash and prefetch-key maps with `with_capacity` + `try_reserve` | Mature, but its default growth allocates; you must assert no growth after init |
| `quick_cache`, `moka`, `foyer` | Off-the-shelf caches | Built for concurrent, dynamically allocated use. They don't give the stash's "must be present" guarantee. Probably not a fit for the commit path |
| `io-uring` | Parallel reads for prefetch workers | Low-level; matches TigerBeetle's model |

### Rust-specific pitfalls

- `HashMap` defaults to SipHash, which is slow for `u128` keys and randomly seeded (nondeterministic iteration). Use a fixed, fast hasher and never iterate a map where order affects the result.
- `HashMap::insert` past capacity silently reallocates. Wrap it so growth past the static bound panics.
- Async temptation: an `async fn commit` "just in case" lets someone add an `.await` on storage later. Keep commit a plain `fn`.
- Holding `&mut` references from the cache across the whole batch fights the borrow checker. Copy the 128-byte value out (`Account: Copy`), modify, and write back, as `create_transfer` does with `dr_account_new`.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt**. Prefetch-then-synchronous-commit is the core of TigerBeetle's single-core speed, and Rust can enforce "no IO in commit" more strongly through types than Zig does by convention.

## 5. Drill questions

### Q1. Recall

List the replica's commit stages in order. Which stage calls `state_machine.commit()`, and which stages can return `.pending`?

> _Your answer:_

### Q2. Mechanism

Why does `create_transfers` prefetch in two IO rounds (transfers, then accounts, then pending transfers) instead of one? What property of a post/void transfer forces this, and what does it imply for any new operation ashi adds?

> _Your answer:_

### Q3. Back-of-envelope

A batch of 8,189 transfers touches 12,000 distinct accounts. 70% are in the object cache, 20% are new ids ruled out by the key-range check, and 10% need a disk lookup averaging 80 µs, with 32 workers in parallel.

- Roughly how long does the account prefetch round take?
- How long would the same misses take if commit read them one by one, inline?

> _Your answer:_

### Q4. What breaks if…

…the stash were removed and the object cache were only the set-associative cache, so prefetched values could be evicted by later inserts in the same batch? Trace what happens in `create_transfer` for a transfer whose debit account was evicted between prefetch and commit, with `constants.verify` off and with it on.

> _Your answer:_

### Q5. ashi design (open-ended)

ashi wants to support a "conditional transfer" whose target account is chosen at execution time from a balance threshold ("credit A if B's balance > X, else credit C"). How would you keep prefetch exact and bounded? Compare three options: prefetch both candidates, a two-phase operation, or rejecting the feature. What does each cost in IO, API complexity and determinism?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- Order: idle → start → check_prepare → **prefetch** → stall → reply_setup → **execute** → checkpoint_durable → compact → checkpoint_data → checkpoint_superblock → idle (`replica.zig:83–118`, `4398–4560`).
- `execute` calls `commit_execute` → `execute_op` → `state_machine.commit()`. It is synchronous and has no `.pending` path.
- `.pending` returns are possible from: start (`commit_start`), prefetch, stall, reply_setup, checkpoint_durable, compact (always `.pending`), checkpoint_data and checkpoint_superblock.

**Q2.**
- A post/void transfer carries only `pending_id`, not the debit/credit account ids. Those are stored in the pending transfer. Prefetch must load the pending transfer (round 1, the transfers groove) before it knows which accounts to load (round 2, the accounts groove).
- Then it loads the `TransferPending` status record by the pending transfer's timestamp (round 3), which was enqueued during round 2's callback.
- The implication: every read must be predictable from the batch plus a **bounded** number of earlier prefetch results. An operation whose read set depends on values computed during execution cannot be prefetched exactly. You would have to over-fetch, add rounds, or redesign it.

**Q3.**
- Disk lookups: `0.10 × 12,000 = 1,200`.
- With 32 in flight: `1,200 / 32 = 37.5` waves × 80 µs ≈ **3 ms**. This assumes the device sustains 32-deep queues at that latency.
- The 20% ruled out by key range and the 70% cache hits cost CPU only, about microseconds each, well under a millisecond in total.
- Inline, one at a time: `1,200 × 80 µs ≈ 96 ms`, during which the core does nothing. At 32× parallelism, prefetch is roughly 32× faster for the IO part. Commit itself then runs in ~8,189 × <1 µs ≈ a few ms of pure CPU.

**Q4.**
- With `constants.verify` **off**: `get_account` misses in `objects_cache`. `Groove.get` has no IO path, so it returns `.not_found`. `create_transfer` returns `.debit_account_not_found` for an account that exists. That is a **silent wrong result**.
- Worse, replicas with different cache histories could diverge. For example, after a restart, one replica's cache contents differ, so evictions differ. That breaks determinism and would show up as diverging state or checksums.
- With `constants.verify` **on**: the check `prefetch_keys.get(.{ .id = ... }).? == .not_found` sees the key was `.found` during prefetch. The assert fails and the replica crashes. That is a liveness failure instead of a safety failure, which is exactly what assertions are for.
- The stash exists to make this impossible: evictions from the set-associative cache "flow into stash", which is cleared only by compaction.

**Q5.** Open-ended. Key points:
- **Prefetch both candidates** (C and A, plus B). Exact and still one round; costs one extra account read per transfer and stays deterministic. Generalizes to "prefetch the union of all branches", which is fine when branches are few and statically known.
- **Two-phase operation:** the client first reads B (lookup), then submits a plain transfer with an expected-balance guard (fail if B changed). This keeps the server simple, but adds a round trip and a failure mode under contention.
- **Rejecting the feature** keeps the invariant "read set = f(batch)" pure.
- A good answer chooses "union of branches" with a hard cap on branch fan-out, encodes that cap in the operation's prefetch-bound declaration, and asserts it at compile time.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
