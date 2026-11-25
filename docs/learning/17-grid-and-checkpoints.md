# Concept 17: Copy-on-write block grid and checkpoints

**Module:** M3 Storage · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/internals/data_file.md` (grid, superblock, "root pointer")
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Overview", "Batching", "Grid" and "Determinism"
- `../tigerbeetle/docs/internals/vsr.md`, sections "Glossary" and "Protocol: Repair Grid"
- `../tigerbeetle/docs/concepts/safety.md`, section "Atomicity"
- `../tigerbeetle/src/constants.zig` (`block_size`, `vsr_checkpoint_ops`, `grid_scrubber_*`)
- `../tigerbeetle/src/config.zig` (`block_size`, `lsm_compaction_ops`, `journal_slot_count`, scrubber and grid defaults)
- `../tigerbeetle/src/vsr/grid.zig` (`GridType`, `create_block`, `read_block_validate`, `checkpoint`, `checkpoint_durable`, `release`)
- `../tigerbeetle/src/vsr/free_set.zig` (`FreeSet`, `Reservation`, `reserve`, `acquire`, `release`, `mark_checkpoint_durable`)
- `../tigerbeetle/src/vsr/checkpoint_trailer.zig` (module comment)
- `../tigerbeetle/src/vsr/superblock.zig` (`CheckpointState`, `checkpoint`, `VSRState.op_compacted`)
- `../tigerbeetle/src/vsr.zig` (`Checkpoint`: `checkpoint_after`, `trigger_for_checkpoint`, `prepare_max_for_checkpoint`, `durable`)
- `../tigerbeetle/src/vsr/replica.zig` (commit stages `checkpoint_data`, `checkpoint_superblock`, `checkpoint_durable`)
- `../tigerbeetle/src/vsr/grid_scrubber.zig` (module comment, tour)
- `../tigerbeetle/src/vsr/message_header.zig` (`Header.Block`), `src/lsm/schema.zig` (`BlockType`)

## 1. The problem

The WAL (Concept 16) is a ring of 1024 slots. It must wrap around and overwrite old prepares. Before it can, the effects of those prepares must be safely stored somewhere else: the LSM forest (Concept 18).

So the replica needs to persist terabytes of state, while:

- surviving a crash at **any** instant, including mid-update,
- never relying on a write being atomic beyond one sector (Concept 14),
- producing **byte-identical** files on every replica, so a corrupt block can be fetched from a peer (Concept 20),
- doing it without stalling the single commit thread (Concept 6).

## 2. The obvious solution, and why it fails

**Update in place.** Store a B-tree on disk and overwrite pages as balances change.

```
page 7: [acct A: 100] ──update──▶ page 7: [acct A: 60]   (same location)
```

If the machine crashes halfway through writing page 7, the page is torn: half old, half new. Neither version survives. The standard fix is a second log: write the new page to a "doublewrite buffer" or log the full page image first (InnoDB, Postgres full-page writes). That doubles write volume.

Worse, a B-tree update often touches several pages (a split rewrites parent and two children). A crash between them leaves the tree structurally broken. You need the WAL to redo or undo multi-page changes, and the recovery code becomes the most complex, least-tested code in the system.

**Checkpoint by flushing dirty pages.** Classic databases periodically flush dirty pages, then record "checkpoint at LSN X". That flush is in place too, so it inherits all the torn-page problems. And it depends on the order the OS writes pages, which brings back the page cache (Concept 14).

## 3. What TigerBeetle does

`data_file.md` summarises the whole idea:

> the grid is used to implement a purely functional, persistent (in both senses), garbage collected data structure which is updated atomically by swapping the pointer to the root node. This is the classic copy-on-write technique commonly used in filesystems.

"Persistent in both senses" means durable on disk *and* persistent in the functional-programming sense: old versions stay intact when new versions are created.

### 3.1 The grid: fixed-size blocks

The grid is the zone after `grid_padding` (Concept 16). It is an array of equal blocks:

```zig
// src/config.zig:161
block_size: comptime_int = 512 * KiB,
```

`constants.zig:586-598` explains the trade-off and the rules:

- "A lower block size increases the memory overhead of table metadata, due to smaller/more tables."
- "A higher block size increases space amplification due to partially-filled blocks."
- `assert(block_size % sector_size == 0)` (Direct IO alignment).
- `assert(block_size <= message_size_max)`, because "Blocks are sent over the network as messages during grid repair and state sync."

Blocks are identified by an **address**, a `u64` starting at 1 (`grid.zig:26-28`). The byte offset is `(address − 1) × block_size` from the grid start (`grid.zig:1579-1583`).

The grid serves every persistent structure. Block types (`src/lsm/schema.zig:58-66`):

| `BlockType` | Holds |
|---|---|
| `free_set` | the encoded free set (which blocks are in use) |
| `client_sessions` | client session table and reply headers |
| `manifest` | the LSM manifest log (table additions/removals) |
| `index` | one LSM table's index: child checksums, keys, addresses |
| `value` | sorted LSM values |

### 3.2 Block headers

Every block begins with the same 256-byte `Header` used for network messages and prepares, specialised as `Header.Block` (`message_header.zig:1560-1586`):

```zig
pub const Block = extern struct {
    pub const metadata_size = 96;
    checksum: u128 = 0,           // covers rest of header (incl. checksum_body)
    checksum_padding: u128 = 0,
    checksum_body: u128 = 0,      // covers the body
    ...
    cluster: u128,
    size: u32 = @sizeOf(Header),  // header + body bytes actually used
    ...
    command: Command,             // always .block
    ...
    metadata_bytes: [metadata_size]u8,  // per-type schema (tree id, back-links, ...)
    address: u64,                 // the block's own address
    snapshot: u64,
    block_type: schema.BlockType,
    reserved_block: [15]u8 = @splat(0),
};
```

Consequences:

- **A block is a valid network message.** `ARCHITECTURE.md` "Grid": "Each `Grid` block is also a valid network `Message`." Repair sends it as-is.
- **Usable payload** per block is `block_size − 256` bytes = 524,032 bytes (`checkpoint_trailer.zig:17`: `chunk_size_max = constants.block_size - @sizeOf(vsr.Header)`).
- **Only used sectors are written.** The grid writes `block[0..sector_ceil(header.size)]` and zeroes the padding first (`grid.zig:966-967`, `1003-1009`). A mostly empty block costs one 4 KiB write, not 512 KiB. Non-zero padding on read is treated as corruption (`.invalid_padding`, `grid.zig:1440-1442`).
- **Self-describing.** `metadata_bytes` encodes the schema (e.g. index block key size and value count), so the scrubber and repair code can decode any block without knowing which tree it belongs to (`schema.zig:1-5`).

Every read is validated against the expected `(address, checksum)` from the parent (Concept 15, `grid.zig:1421-1446`).

### 3.3 The grid cache

Since Direct IO bypasses the kernel page cache, TigerBeetle has its own: a set-associative block cache (`grid.zig:14`), sized by `grid_cache_size_default = GiB` (`config.zig:132`). Reads choose a `CachePolicy` (`grid.zig:79-86`). When a block is released, it is demoted in the cache "to reduce conflict misses" but stays readable "until the next checkpoint" (`grid.zig:705-711`).

Concurrency is bounded: `grid_iops_read_max = 32` and `grid_iops_write_max = 32` (`config.zig:130-131`). Excess requests wait in an intrusive queue (Concept 12).

### 3.4 Copy-on-write: never overwrite a live block

The rule: **a block that is reachable from the current durable checkpoint is never overwritten.** New data goes to free blocks. Old blocks are released, but not reused until it is provably safe.

```
checkpoint C_k reachable:          after compaction (in progress):

superblock ─▶ manifest M1          superblock ─▶ manifest M1          (on disk, unchanged)
               └▶ index I1                        └▶ index I1
                   └▶ value V1                        └▶ value V1
                   └▶ value V2                        └▶ value V2

                                   in memory:  manifest M2 (new address)
                                                 └▶ index I2 (new) ─▶ V1 (shared), V3 (new)
                                   released:   I1, V2   (still intact on disk!)
```

`create_block` enforces this with assertions: the address must be acquired (`!grid.free_set.is_free(...)`), not currently being written, and not being read (`grid.zig:918-937`). Each write records the `checkpoint_id` at start, so the grid can assert "the checkpoint does not advance during the (non-repair) write" (`grid.zig:49-60`).

If the replica crashes mid-compaction, the half-written new blocks are simply unreachable. The old tree, reachable from the superblock, is untouched.

### 3.5 The free set: acquire, release, free

`FreeSet` (`free_set.zig:45-113`) tracks block state with bitsets:

- `blocks_acquired`: set bit = block is in use.
- `blocks_released`: set bit = released during the current checkpoint interval, "to be freed when the next checkpoint becomes durable".
- `blocks_released_prior_checkpoint_durability`: releases that happen before the *current* checkpoint is durable (explained below).
- `index`: one bit per **shard** of 4096 blocks; set = shard is full. "Each shard is 8 cache lines because the CPU line fill buffer can fetch 10 lines in parallel" (`free_set.zig:100-113`). Finding a free block scans the small index first, then one shard.

A block's life:

```
        acquire()                release()                 next checkpoint durable
free ──────────────▶ acquired ─────────────▶ released ─────────────────────────▶ free
                     (written,               (still on disk,
                      reachable)              still reachable from C_k)
```

**Reservations make allocation deterministic.** Several compaction jobs run concurrently and finish in a nondeterministic order. If they grabbed blocks first-come-first-served, replicas would place the same data at different addresses, breaking byte-identical grids. The fix (`free_set.zig:29-44`):

1. **Reserve**, in deterministic order: each job reserves an upper bound of blocks (`reserve(count)` returns a range).
2. **Acquire**: jobs run concurrently but each acquires only from its own range.
3. **Forfeit**: when done, the job drops its reservation. When all are forfeited, unused reserved space returns to the pool.

Allocation is "first free" from the start of the bitset (`free_set.zig:347-383`), which also keeps the file compact.

**Why release is delayed past a checkpoint.** A block released during interval *k* is still part of checkpoint C_k. If the replica crashes before C_{k+1} is written, it restarts from C_k and needs that block. So it can't be freed at least until C_{k+1} exists.

TigerBeetle waits even longer: until C_{k+1} is **durable on a commit quorum** of replicas. `free_set.zig:524-527`:

> These blocks must not be freed till the current checkpoint is durable, so as to maintain the durability of these blocks on a commit quorum of replicas.

The reason is the cluster, not just this replica. A lagging replica may still be on C_k and need to repair one of C_k's blocks from us (Concept 25). Once a commit quorum has C_{k+1}, C_k is no longer needed by anyone who matters.

`mark_checkpoint_durable` (`free_set.zig:600-622`) then frees everything in `blocks_released`, and moves releases that happened in the meantime (`..._prior_checkpoint_durability`) into `blocks_released` for next time.

### 3.6 Checkpoint trailers: storing the free set in the grid

The free set and client sessions are too big for the superblock, so they are written into grid blocks called **trailers**. `checkpoint_trailer.zig:40-56`:

- The free set is stored "as a linked list of blocks containing EWAH-encoding of a bitset of acquired blocks." EWAH is a run-length compressed bitset. "The length of the linked list is proportional to the degree of fragmentation, rather that to the size of the data file. The common case is a single block."
- The trailer blocks hold the free set, yet they are themselves blocks that need to be in the free set. The code resolves this by marking them free in the encoding and patching them in after decoding: "This might or might not be related to Russell's paradox."
- After the superblock is written, the old trailer blocks are released like any other (`replica.zig:5283-5288`).

### 3.7 The checkpoint: an atomic superblock flip

A checkpoint makes a new root durable. Two commit stages do it (`replica.zig:4502-4527`):

**`checkpoint_data`** (`replica.zig:5013-5071`). In parallel:
- the state machine (forest) flushes its manifest log,
- client sessions are encoded into their trailer,
- client replies are flushed,
- the grid encodes and writes the free set trailers (`grid.checkpoint`, `grid.zig:482-508`). This must come last because "it encodes the free set" after everyone else has acquired their blocks.

**`checkpoint_superblock`** (`replica.zig:5134-5200` and `superblock.zig:899-990`). It builds a new `CheckpointState` with the new manifest, free set and client session references, the new `storage_size`, `parent_checkpoint_id` and the checkpoint's prepare header. It then writes the 4 superblock copies sequentially and verifies 3 of 4 (Concept 16).

```
time ──────────────────────────────────────────────────────────────▶
 grid:  [new blocks written to free addresses] [trailer blocks]
 superblock:                                                [copy0][copy1][copy2][copy3][verify]
                                                                    ▲
 crash anywhere before here ─▶ open finds seq s (≥2 copies)  ──▶ old root, old blocks intact
 crash after ≥2 copies of s+1 ─▶ open finds seq s+1           ──▶ new root, new blocks intact
```

That is the **atomic flip**. Everything the new root references was written before the first superblock copy. Nothing the old root references was overwritten. Whichever version the quorum logic picks, it is complete.

After the superblock callback, the new checkpoint is marked **not yet durable** (`grid.mark_checkpoint_not_durable`, `replica.zig:5286`). It becomes durable later, as described next.

### 3.8 The checkpoint interval

Checkpoints happen on a fixed op schedule, the same on every replica (determinism again). The constants (`config.zig:155-164`): `journal_slot_count = 1024`, `lsm_compaction_ops = 32` (one "bar"), `pipeline_prepare_queue_max = 8`.

```zig
// src/constants.zig:40-42
pub const vsr_checkpoint_ops = journal_slot_count -
    lsm_compaction_ops -
    lsm_compaction_ops * stdx.div_ceil(pipeline_prepare_queue_max * 2, lsm_compaction_ops);
```

= 1024 − 32 − 32 × ⌈16 / 32⌉ = 1024 − 32 − 32 = **960 ops**.

Why not 1024? The WAL must never overwrite an entry that a crashed replica might still need for replay. The comment at `constants.zig:45-68` lists the margins:

- `lsm_compaction_ops` (32): the last bar before a checkpoint trigger is still in memory, not on disk. It must survive in the WAL until the *next* checkpoint.
- `2 × pipeline_prepare_queue_max` (16, rounded up to a bar = 32): room for ops prepared after the trigger while the checkpoint is being written, plus enough that a quorum is guaranteed to have reached the checkpoint before old WAL entries are overwritten.

The arithmetic in `src/vsr.zig:1652-1707`:

| Function | Rule | Production values |
|---|---|---|
| `checkpoint_after(c)` | first: `vsr_checkpoint_ops − 1`; then `c + vsr_checkpoint_ops` | 959, 1919, 2879, … |
| `trigger_for_checkpoint(c)` | `c + lsm_compaction_ops` | 991, 1951, 2911, … |
| `prepare_max_for_checkpoint(c)` | `trigger + 2 × pipeline_prepare_queue_max` | 1007, 1967, 2927, … |
| `durable(c, commit)` | `commit > trigger + pipeline_prepare_queue_max` | durable once commit ≥ 1000, 1960, … |

Walk through the first checkpoint:

```
op:   1 ............ 959 | 960 ...... 991 | 992 ... 999 | 1000 ... 1007
      ─── on disk at checkpoint ───│── in memory ──│
                                   ▲ checkpoint op = 959
                                                   ▲ trigger: commit of op 991 writes the checkpoint
                                                                 ▲ op 1000 committed: checkpoint durable,
                                                                   blocks released in interval 0 are freed
                                                                               ▲ prepare max (1007)
```

When op 991 commits, the replica writes a checkpoint whose state covers ops up to **959**. Ops 960 to 991 are still in the in-memory table; they reach disk via compaction during the next bar. The superblock comment in `replica.zig:5150-5159` illustrates this with a small WAL. Until that checkpoint exists, the replica may not prepare past op 1007 (`op_prepare_max`, `replica.zig:7104-7110`). Ops 1 to 1007 fit in 1024 slots with room to spare. Steady state: the WAL must hold ops 960 to 1967, which is 1008 ≤ 1024.

The durability rule follows from `prepare_ok` limits: a replica won't acknowledge prepares beyond trigger + pipeline until it has itself checkpointed. So if op trigger + pipeline + 1 is committed, a quorum must have checkpointed. `replica.zig:4999-5006` asserts that each replica marks the checkpoint durable at exactly `commit_min == trigger + pipeline_prepare_queue_max + 1`, "crucial for storage determinism".

> ⚠️ **Docs vs code.** `ARCHITECTURE.md` "Batching" says "Changes from 1024 prepares are aggregated before the checkpoint is atomically advanced by overwriting the superblock". The code's interval is `vsr_checkpoint_ops` = **960** with production constants, not 1024. 1024 is the WAL length (`journal_slot_count`); the checkpoint interval is deliberately shorter by 64 ops. The same section's "Changes from 32 prepares are aggregated in memory" matches `lsm_compaction_ops = 32`.

### 3.9 Recovery: replay the WAL from the last checkpoint

`safety.md` "Atomicity": "If TigerBeetle crashes, the WAL is replayed at startup from the last checkpoint on disk."

At startup the replica does the following. Steps 2 to 4 are interleaved in `replica.zig`'s open path; the list shows the dependencies, not exact ordering.

1. Opens the superblock: reads 4 copies, picks the quorum (Concept 16). This gives `CheckpointState`.
2. Opens the grid's free set by reading the trailer blocks referenced from the superblock.
3. Opens the forest by replaying the manifest log from the manifest blocks referenced from the superblock.
4. Recovers the WAL: reads both rings, classifies each slot (Concept 16 and 24).
5. Re-executes committed prepares after the checkpoint op, in order, verifying the hash chain (Concept 15).

Because execution and block allocation are deterministic, replay produces exactly the same in-memory state and writes exactly the same blocks to the same addresses as before the crash (`data_file.md`: "replaying the operations after the crash results in exactly the same on-disk and in-memory state").

A subtle detail: ops between the checkpoint and its trigger (960 to 991 above) were in memory at the checkpoint, but their *compaction* work in the following bar was already persisted. Replaying them must not redo that compaction. `VSRState.op_compacted` (`superblock.zig:305-314`) tracks this: "the corresponding `compact()` updates were preserved, and must not be repeated to ensure deterministic storage."

Reads during recovery or normal operation never fail. A read can use `from_local_or_global_storage` (`grid.zig:68-75`): if the local copy is bad, the read waits until the block is repaired from another replica. `ARCHITECTURE.md` "Determinism": the read function "doesn't have an error condition."

### 3.10 The grid scrubber

Blocks that are rarely read (cold LSM levels) could rot unnoticed for months. If a peer's copy also rots, the data is lost. `grid_scrubber.zig:1-19`:

> The most common type of disk fault is a latent sector error ... "An Analysis of Latent Sector Errors in Disk Drives" (2007) found that >60% of latent sector errors were discovered by a scrubber that cycles every 2 weeks.
>
> Finding and repairing errors proactively minimizes the risk of cluster data loss due to multiple intersecting faults (analogous to a "double-fault") ...

How it works:

- A **tour** visits every acquired block: LSM tables (index block, then its value blocks), the manifest log in reverse order, the free set trailers and the client sessions trailer (`grid_scrubber.zig:105-129`).
- Each replica starts its tour at an independently chosen random origin, so the same block isn't scrubbed at the same time everywhere (`grid_scrubber.zig:181-183`).
- Pacing: one full tour per `grid_scrubber_cycle = 180 days` (`config.zig:138`), with reads spaced between 50 ms and 10 s (`config.zig:139-140`), `grid_scrubber_reads_max = 1` (`config.zig:136`). `constants.zig:388-393` does the arithmetic for a full 64 TiB file: about 4.32 MiB/s.
- A failed scrub queues the block for repair from peers. Blocks freed by a checkpoint mid-scrub are marked `released` and skipped (`grid_scrubber.zig:65-71`).

The WAL and superblock are not covered by the grid scrubber; they are read and verified at startup and during normal operation.

### 3.11 Other stale comments

> ⚠️ **Docs vs code (minor).**
> - `free_set.zig:105` sizes its example with "64KiB/block". The production `block_size` is 512 KiB.
> - `free_set.zig:407-408` says a block can't be re-acquired until "the release has been checkpointed", and `grid.zig:717-719` says a released block "can only be overwritten in the next checkpoint". The code is stricter: freed only when the *next checkpoint becomes durable* on a commit quorum (`free_set.zig:524-531`, `600-617`).
> - `data_file.md` says the superblock references a bitset "of all grid blocks which are not currently allocated". The code stores two EWAH bitsets, of *acquired* blocks and of *released* blocks (`superblock.zig:506-536`).

## 4. What it means for ashi

### Rust features that help

- **Immutability is the default.** A written block is never mutated. In Rust, hand out `&[u8; BLOCK_SIZE]` or an index into a static block pool. The borrow checker helps stop writers touching a block that readers hold.
- **Typed state transitions.** Model block lifecycles with types or enums (`Free`, `Acquired`, `Released`) and make `release` and `free` take the right states. Reservations can be a non-`Clone`, non-`Copy` token that must be passed to `forfeit` (a "must-use" linear-ish type), like TigerBeetle's `session` check.
- **`#[must_use]`** on `Reservation` catches forgotten forfeits.

### Patterns

- **Static block pool, not `Arc<Block>`.** TigerBeetle uses a fixed pool with manual reference counts (`grid.zig` `block_ref`/`block_unref`). In Rust, a `Vec<AlignedBlock>` allocated once plus `u32` indices avoids per-block allocation and keeps everything Direct IO aligned (Concept 11).
- **Bitset with a shard index.** `Vec<u64>` for blocks plus a smaller `Vec<u64>` of "shard full" bits. Use `(!word).trailing_zeros()` to find a free bit. 64 TiB / 512 KiB = 134,217,728 blocks = 16 MiB per bitset; the index is 4 KiB.
- **Checkpoint as an ordered pipeline:** data writes → trailers → superblock copies one by one → verify → mark not durable → (later) durable → free.
- **Record the checkpoint generation in every in-flight read and write** and assert it didn't change, as TigerBeetle does with `checkpoint_id`.

### Crates

| Crate | Use | Trade-off |
|---|---|---|
| hand-rolled `Vec<u64>` | free set bitset | trivial, fully deterministic, zero deps |
| `bitvec` | bitset utilities | heavier API; you need only find-first-unset |
| `roaring` | compressed bitset for the on-disk free set | well tested, deterministic serialisation, but a different format from EWAH and a dependency |
| `indexmap` | insertion-ordered map (like Zig's `AutoArrayHashMap`) | needed only if you mirror `blocks_released_prior_checkpoint_durability` |

### Pitfalls specific to Rust

- **`HashMap` iteration order is random** (`RandomState`). If anything that affects allocation order iterates a `HashMap`, replicas diverge and physical repair breaks (Concept 20). Use bitsets, `BTreeMap`, `IndexMap`, or a fixed hasher.
- **Async tasks finishing in any order.** With `tokio` or any executor, concurrent compaction jobs complete nondeterministically. You *must* reserve in a fixed order before spawning, exactly like TigerBeetle.
- **`Drop` running IO.** Don't let a block guard's `Drop` release or write anything. Releases must happen at deterministic points in the commit pipeline.
- **Holding references across a checkpoint.** A `&Block` taken before a checkpoint could point to a block freed after the checkpoint becomes durable and then reused. Tie block handles to a checkpoint generation, or make it impossible to hold them across commit stages.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt** copy-on-write with deterministic reservations and durability-gated freeing. It is the foundation of physical repair, which ashi needs to match TigerBeetle's fault tolerance. **Adapt** sizes (block size, checkpoint interval) only after benchmarking.

## 5. Drill questions

### Q1. Recall

What is the grid block size, how many bytes of each block are usable payload, and what are the five block types? Where is a block's expected checksum stored?

> _Your answer:_

### Q2. Mechanism

Explain why a block released during checkpoint interval *k* is not freed when checkpoint C_{k+1} is written, but only when C_{k+1} becomes durable on a commit quorum. Name the exact commit number at which a replica marks the first checkpoint durable in production.

> _Your answer:_

### Q3. Back-of-envelope

Derive `vsr_checkpoint_ops` from the production constants. Then: with a full 64 TiB data file, how big is the uncompressed `blocks_acquired` bitset, and how many shard-index bits are there? At the scrubber's default cycle, roughly how many 512 KiB blocks does it read per second?

> _Your answer:_

### Q4. What breaks if…

…the free set allocated blocks first-come-first-served as compaction jobs asked for them, without reservations? The cluster has 6 replicas and all are healthy. Describe the first time this would cause a visible failure.

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle's checkpoint interval (960 ops) is tied to the WAL length (1024 slots). If ashi wanted checkpoints less often (to amortise superblock and trailer writes), what would it have to change, and what does it cost? Consider WAL size on disk, recovery time, and how long released blocks stay unusable.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** `block_size = 512 KiB` (`config.zig:161`). Payload = 512 KiB − 256 B header = 524,032 bytes. Types: `free_set`, `client_sessions`, `manifest`, `index`, `value` (plus `reserved = 0`, never written). The expected checksum is stored *outside* the block, in its parent: the index block for value blocks, the manifest for index blocks, the previous block for manifest and trailer chains, and the superblock for the chain heads.

**Q2.** A block released in interval *k* is still reachable from C_k. Writing C_{k+1} makes this replica independent of C_k, but other replicas may still be on C_k (lagging, or recovering) and may need to fetch that block from us to repair their copy. Freeing it early could mean the cluster loses the only good copy. Once a commit quorum has C_{k+1}, the cluster can always recover from C_{k+1}, so C_k's released blocks are safe to reuse. Production: first checkpoint op 959, trigger 991, durable when `commit > 991 + 8`, so the replica marks it durable when committing op **1000** (`commit_min == trigger + pipeline_prepare_queue_max + 1`).

**Q3.** `1024 − 32 − 32 × ⌈(8 × 2) / 32⌉ = 1024 − 32 − 32 = 960`. 64 TiB / 512 KiB = 2²⁷ = 134,217,728 blocks → 2²⁷ bits = 16 MiB for `blocks_acquired` (same again for `blocks_released`). Shards of 4096 bits → 2²⁷ / 2¹² = 32,768 index bits = 4 KiB. Scrubber: `constants.zig` computes ~4.32 MiB/s for 64 TiB over 180 days. 4.32 MiB/s ÷ 0.5 MiB ≈ 8.6 blocks per second (in practice blocks are only partly filled, so bytes read are fewer).

**Q4.** Compaction jobs finish IO in different orders on different replicas (disk latency varies). Without reservations, replica A might put a new value block at address 500 while replica B puts it at 501. Logically identical, physically different. Nothing fails at first: each replica's own tree is self-consistent. The first visible failure comes when a block is corrupt on one replica and it asks a peer for `(address=500, checksum=X)`. The peer has a different block at 500, so it can't answer. Grid repair and state sync, which depend on byte-identical grids, stop working. Under the VOPR's fault injection this would be caught quickly. The checkpoint ids (which hash the free set references) would also differ, so divergence is detected at the next checkpoint (`checkpoint_id` in prepares, Concept 15).

**Q5.** A longer interval needs a longer WAL. The WAL must hold `vsr_checkpoint_ops + lsm_compaction_ops + 2 × pipeline` ops (`constants.zig:69-70`). Doubling the interval roughly doubles `journal_slot_count`, so the WAL prepares zone goes from 1 GiB to 2 GiB (at 1 MiB slots). Recovery replays up to a whole interval of prepares, so worst-case restart time roughly doubles. Released blocks stay unusable for up to two intervals, so space amplification grows under heavy update load. Benefits: fewer superblock and trailer writes, fewer free-set encodes. Since superblock writes are small (8 KiB × 4 copies), the savings are mostly in trailer writes and the checkpoint pause. The case for changing TigerBeetle's choice is probably weak unless measurements show checkpoint stalls.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
