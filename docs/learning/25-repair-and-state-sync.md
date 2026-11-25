# Concept 25: Repairing the write-ahead log and the grid; state sync

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/internals/sync.md`, all sections ("Glossary", "Algorithm", "0: Scenarios", "1: Triggers", "5: Conclusion", "Syncing Replica", "Checkpoint Identifier", "Storage Determinism")
- `../tigerbeetle/docs/internals/vsr.md`, sections "Commands", "Protocol: Request/View", "Protocol: Repair Journal", "Protocol: Repair WAL", "Protocol: Repair Client Replies", "Protocol: Repair Grid", "Protocol: Sync Client Replies", "Protocol: Sync Forest"
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Determinism" and "Hash-Chaining"
- `../tigerbeetle/src/vsr/sync.zig` (whole file, 29 lines)
- `../tigerbeetle/src/vsr/grid_blocks_missing.zig`: header comment (1–11), `FaultyBlock` (32–61), `state` (128–144), `init` (147), `repair_block` (274), `sync_table` (295)
- `../tigerbeetle/src/vsr/repair_budget.zig`: `RepairBudgetJournal` (12–260), `RepairBudgetGrid` (262–464)
- `../tigerbeetle/src/vsr/grid_scrubber.zig`: header comment (1–20), `tour` and origin (105–190)
- `../tigerbeetle/src/vsr/grid.zig`: `ReadBlockCallback` (68–75), `Read.coherent` (95–104), `read_global_queue` (230–239), `read_block_validate` (1421–1446)
- `../tigerbeetle/src/vsr/client_replies.zig`: header comment (1–20), `faulty` (88–94)
- `../tigerbeetle/src/vsr/replica.zig`: `on_get_prepare` (3053), `on_get_reply` (3210), `on_get_blocks` (3321), `on_block` (3451), `on_repair_sync_timeout` (3800), `on_grid_repair_timeout` (3836), `on_view_set_checkpoint` (2848–2906), `op_repair_min`/`op_repair_max` (7220–7282), `repair` (7560–7800), `repair_header_would_connect_hash_chain` (7963–7998), `repair_prepare` (8341), `repair_stuck` (8490), `sync_start_from_committing` (10385), `sync_enqueue_tables` (10778), `send_get_blocks` (11272)
- `../tigerbeetle/src/vsr/superblock.zig`: `VSRState.sync_op_min/max` (121–131), `CheckpointState` (320–411), `checkpoint_id` (444)
- `../tigerbeetle/src/vsr.zig`: `Command` (251–291), `Checkpoint.durable` (1699)
- `../tigerbeetle/src/constants.zig` (40–72, 178, 330–405, 418–420), `../tigerbeetle/src/config.zig` (104–160)

## 1. The problem

A replica can end up missing data in many ways:
- **Gaps:** it missed some prepares because the network dropped them (Concept 28).
- **Corrupt WAL entries:** recovery marked some slots faulty (Concept 24).
- **Corrupt grid blocks:** a latent sector error in the LSM data (Concept 18). This is often found only when compaction reads the block.
- **Lost client replies:** the cached reply for a client is corrupt.
- **Far behind:** it was down for a while. Meanwhile the cluster went through more than a full lap of the WAL ring, so the entries it needs have been overwritten everywhere.

Each of these quietly reduces fault tolerance. A 6-replica cluster where two replicas are "up" but can't catch up is really a 4-replica cluster. For a ledger, the next fault could then lose availability, or data.

## 2. The obvious solution, and why it fails

**"Resend the log from the start."** The WAL is a ring of `journal_slot_count = 1024` slots (`config.zig:158`). Old entries are overwritten after each checkpoint. There *is* no log from the start.

**"Logical state transfer": dump every account and transfer and replay them.** The default `storage_size_limit_default` is 16 TiB (`config.zig:98`). At 1 GB/s that is about 16,000 s, roughly 4.5 hours, during which the cluster runs one replica short. And for a single bad byte in one LSM block, logical repair "might entail re-transmitting the entire tree" (`ARCHITECTURE.md`, "Determinism").

**"Copy the bad block from a peer."** This only works if the peer has *the same bytes at the same address*. With ordinary databases each replica compacts on its own schedule, so block 12,345 on replica A has nothing to do with block 12,345 on B.

**"Ask every peer for everything you're missing."** A lagging replica floods the healthy ones. An asymmetrically partitioned replica (can send, can't receive) keeps asking forever and starves the cluster's disk reads.

## 3. What TigerBeetle does

### 3.1 The repair protocols at a glance

Every repair is a request/response pair of messages (`vsr.zig:251–291`; `vsr.md`, "Commands"):

| Protocol | Request | Response | Repairs |
|---|---|---|---|
| Repair Journal | `get_headers` (14) | `headers` (17) | missing or disconnected WAL headers |
| Repair WAL | `get_prepare` (15) | `prepare` (6) | missing or corrupt prepares |
| Repair Client Replies | `get_reply` (16) | `reply` (8) | corrupt cached replies |
| Repair Grid / Sync Forest | `get_blocks` (19) | `block` (20) | corrupt or missing grid blocks |
| Request/View (sync trigger) | `get_view` (13) | `view` (24) | head op, and checkpoint for state sync |

> The command for grid repair is `get_blocks` in this version. Retired ordinals are kept so they can't be reused: `deprecated_21 = 21, // .request_sync_checkpoint` and `deprecated_22 = 22, // .sync_checkpoint` (`vsr.zig:288–289`).

All of these are **fire-and-forget**. A peer that can't help simply doesn't answer. The requester retries on a timeout. This matches the weak network contract of Concept 28.

### 3.2 WAL repair: headers backwards, prepares forwards

`Replica.repair` (`replica.zig:7560–7572`) runs these steps in order:

```
1. If we are a backup and have fallen too far behind, initiate state sync.
2. Advance the head op to op_repair_max = min(op_prepare_max, commit_max),
   by requesting a View (whose "hook" headers bridge us forward).
3. Acquire missing or disconnected headers in REVERSE chronological order,
   backwards from op_repair_max.
4. Repair missing or corrupt prepares in CHRONOLOGICAL order.
5. Commit up to op_repair_max. If that triggers a checkpoint, go to step 1.
```

**Why headers go backwards.** Prepares are hash-chained (Concept 15): each header's `parent` field is the checksum of the previous op's header (`message_header.zig`, `Prepare.parent`). If you trust the header at op N, you can verify op N−1 by checking `header[N].parent == checksum(header[N-1])`. Then you can verify N−2, and so on.

```
   trusted head (from a View message from the primary)
                                      ▼
 op:  96      97      98      99     100
     [h96]◄──[h97]◄──[h98]◄──[h99]◄──[h100]
        parent  parent  parent  parent
   a header received from ANY replica is accepted
   only if it links into this chain
```

So repaired headers can come from *any* replica, even a buggy or stale one. `repair_header_would_connect_hash_chain` (`replica.zig:7975`) walks forward from a candidate header to `self.op` and accepts it only if every link matches. This is what makes it "safe for both backups and transitioning primaries" (`vsr.md`).

**Why prepares go forwards.** Per `vsr.md`, chronological order "improves the chances that older entries will be available, i.e. not yet overwritten" and "enables better pipelining of repair and commit". In code, `repair` first repairs `[commit_min+1, repair_op_max]` to keep committing, then `[op_repair_min, commit_min]` to restore durability of already-committed entries "at risk of getting evicted from the journal" (`replica.zig:7712–7725`).

**Don't repair too eagerly.** Normally `repair_op_max = op - pipeline_prepare_queue_max` (8), because missing ops near the head "may arrive via normal replication". Every 50 timeouts it repairs everything anyway, so an idle cluster still heals (`replica.zig:7651–7666`).

**Serving a `get_prepare`.** A peer answers from its in-memory pipeline first ("messages in the pipeline are never corrupt"). Otherwise it reads from disk using `prepare_checksums`, which "may have the prepare we want — even if journal recovery marked the slot as faulty" (`replica.zig:3076–3110`).

**Disk reads are reserved.** The journal has `journal_iops_read_max = 8` reads. Two are reserved for the commit path, leaving 6 for repair, "so that an asymmetrically partitioned replica cannot starve the cluster with get_prepare messages" (`journal.zig:97–103`).

### 3.3 The repair budget

Repair requests are rate-limited per destination replica (`repair_budget.zig`).

**`RepairBudgetJournal`** (for `get_prepare`):
- At most `repair_messages_inflight_count_max = 2` in-flight requests per remote replica (`repair_budget.zig:57`). The reason: the remote's outgoing `send_queue` holds 4 messages (`connection_send_queue_max_replica = @max(@min(clients_max, 4), 2)`, `constants.zig:420`). Overflowing it drops messages and wastes disk reads.
- Pick the replica with the **lowest repair latency**, tracked as an exponentially weighted moving average: `new = (old*4 + sample)/5` (`repair_budget.zig:255–259`).
- With probability `experiment_chance = 1/10`, pick a random eligible replica instead (`repair_budget.zig:35`). Otherwise a crashed replica that once had the best latency could be chosen forever.
- A request expires after `min(2 × latency, 500 ms)` (`duration_expiry_max`, `repair_budget.zig:48`). Expiry refunds the budget and counts the wait toward that replica's latency.

**`RepairBudgetGrid`** (for `get_blocks`):
- Per replica: `grid_repair_request_max + 1 = 5` blocks in flight (`repair_budget.zig:286`; `grid_repair_request_max = 4`, `config.zig:133`).
- A request expires after 250 ms; the same block is not re-requested within 100 ms (`repair_budget.zig:272–276`).
- The destination is a **random** replica with at least `grid_repair_request_max` budget left (`next_destination`, `repair_budget.zig:341`).

One more detail shows how deep determinism goes. On receiving a block, the budget is refunded for *every* replica it was requested from. The comment explains why: "We have no information about the replica that sent this block, as storing each replica's index in the block header would make storage non-deterministic" (`repair_budget.zig:414–425`).

### 3.4 Grid repair

Grid blocks are addressed by `(address, checksum)`. The checksum is stored in the *parent*, not in the block (Concept 15). So a reader always knows exactly which bytes it expects.

**The read path** (`grid.zig:68–75`) offers two callbacks:
- `from_local_storage`: if the local read fails, report the error. Used when serving *other* replicas.
- `from_local_or_global_storage`: if the local read fails, park the read in `read_global_queue`. "The callback passed to this function won't be called until the block has been recovered." Used by compaction and queries.

That second mode is why `ARCHITECTURE.md` can say "the function that reads data from storage doesn't have an error condition". The LSM code never handles a read error. A bad block just takes longer to arrive.

`read_block_validate` (`grid.zig:1421–1446`) rejects a block if the header checksum fails, `command != .block` (misdirected IO), the body checksum fails, the checksum is not the one expected, or the sector padding is not zero.

**The protocol:**

```
 repairing replica R                         peer P
 ───────────────────                         ──────
 local read fails → read_global_queue
 or GridBlocksMissing.repair_block()
 on_grid_repair_timeout / repair():
   send_get_blocks(P) ── get_blocks{[(addr,cksum) × ≤4]} ──►
                                             on_get_blocks: for each request,
                                               grid.read_block(from_local_storage,
                                                               cache_lookup_no_admit)
                        ◄──────── block (the raw grid block) ─ if .valid
 on_block: write block to grid,
   resolve parked reads, refund budget
```

`send_get_blocks` (`replica.zig:11272`) splits the at-most-4 slots between stalled reads (so commit and compaction can continue) and `GridBlocksMissing` entries, so neither starves.

**A grid block *is* a message.** Every block starts with a `vsr.Header` whose `command = .block` (`message_header.zig`, `Header` doc: "Network message, prepare, and grid block header"). Serving a block means copying it into a message buffer and sending it (`replica.zig:3433–3446`). `block_size = 512 KiB` (`config.zig:161`) fits inside `message_size_max = 1 MiB`.

**`GridBlocksMissing`** (`grid_blocks_missing.zig`) tracks what is still needed. It is:
- LSM-aware: it can repair whole tables (index block, then the value blocks it lists).
- Coherent: everything in it belongs to the current checkpoint. It won't repair freed blocks.
- Bounded: `grid_missing_blocks_max = grid_repair_writes_max + grid_scrubber_writes_max` = 32 + 1 = 33 single blocks (`constants.zig:353`), plus `grid_missing_tables_max = 6` tables (`config.zig:135`).

Each entry records its cause, `repair { scrub }` or `sync { table, block }` (`grid_blocks_missing.zig:43–60`).

### 3.5 The grid scrubber

Waiting for compaction to stumble on a bad block is too late. If a peer's copy has *also* gone bad by then, that is a double fault. `grid_scrubber.zig:1–20` cites a study that found ">60% of latent sector errors were discovered by a scrubber that cycles every 2 weeks."

TigerBeetle's scrubber:
- Walks the LSM forest and manifest blocks (it is LSM-aware, not a raw sweep of addresses).
- Starts each replica's tour at an independently chosen random origin, "to minimize risk of data loss" (`grid_scrubber.zig:134–136, 181–190`). Replicas then don't all scrub the same region at the same time.
- Default cycle `grid_scrubber_cycle = 180 days`, 1 read at a time, interval clamped to 50 ms–10 s (`config.zig:136–140`).
- Napkin math from `constants.zig:385–393`: 64 TiB / 180 days ≈ **4.32 MiB/s** worst-case read overhead.
- Feeds faults into `GridBlocksMissing.repair_block(.., .{ .scrub = true })`.

### 3.6 Client-reply repair

Each replica stores the latest reply for every client session (Concept 27):
- The headers live in the client-sessions trailer.
- The bodies live in the `client_replies` zone: `client_replies_size = clients_max * message_size_max` = 64 × 1 MiB = 64 MiB (`constants.zig:178`).
- Only non-empty replies are written (`client_replies.zig:7–8`).

A corrupt reply sets a bit in `ClientReplies.faulty` (`client_replies.zig:88–94`). `repair` then sends `get_reply{client, op, checksum}` to a random other replica (`replica.zig:7746–7762`). The peer answers only if its stored reply has exactly that checksum (`replica.zig:3210–3259`).

If every replica loses the same reply *and* the client never received it, the client is "locked out", retrying forever. The fix is to restart the client, which starts a new session (`client_replies.zig:16–20`).

### 3.7 State sync: when the WAL has moved on

**How far is too far?** A checkpoint happens every `vsr_checkpoint_ops` ops (`constants.zig:40–42`):

```
vsr_checkpoint_ops = journal_slot_count
                   - lsm_compaction_ops
                   - lsm_compaction_ops * ceil(pipeline_prepare_queue_max * 2 / lsm_compaction_ops)
                   = 1024 - 32 - 32 * ceil(16/32) = 960
```

The rules (`sync.md`, "0: Scenarios"):
- More than one checkpoint behind the primary → must state sync.
- Same checkpoint → WAL repair only.
- Exactly one behind → depends. State sync is wrong if only one replica is on the next checkpoint (its state could be corrupt). WAL repair is wrong if peers have already overwritten the needed prepares. In short: once the next checkpoint is *durable*, the lagging replica must eventually sync.

"Durable" has a precise definition: `Checkpoint.durable(checkpoint, commit)` is `commit > trigger + pipeline_prepare_queue_max` (`vsr.zig:1699–1707`). It works because syncing replicas withhold `prepare_ok` until their checkpoint is replicated (`sync.md`, "Syncing Replica"). So commits beyond that point prove a replication quorum has the checkpoint.

**Trigger.** `on_view_set_checkpoint` (`replica.zig:2848–2906`) starts sync when a `view` message carries a durable, newer checkpoint and either:
- the cluster is ≥ 2 checkpoints ahead (`far_behind`), or
- repair is `likely_stuck`: `repair_stuck()` sees no commit progress, plus a broken hash chain, a dirty next prepare, or a stalled grid read (`replica.zig:8490–8518`).

A backup that is stuck also asks for that view itself, from `repair_sync_timeout` (`replica.zig:3800–3833`).

**Stages** (`sync.zig`):

```
idle ──► canceling_commit ──► canceling_grid ──► updating_checkpoint ──► idle
  └───────────────────────────────▲
   (if the commit stage is interruptible, skip straight to canceling_grid)
```

**Algorithm** (`sync.md`, "Algorithm"):
1. Interrupt the commit pipeline; cancel grid reads.
2. Write the new `CheckpointState` and view headers into the superblock. Set `sync_op_min`/`sync_op_max` to the range of ops we skipped (`superblock.zig:121–131`).
3. **Lazily** fetch what the new checkpoint points to, using the grid repair protocol:
   - free set, client sessions and manifest blocks;
   - table blocks (Sync Forest);
   - client replies (`get_reply`).
4. At the *next* checkpoint, reset `sync_op_min = sync_op_max = 0`.

"State sync is lazy — logically, sync is completed when the superblock is synced. The data pointed to by the new superblock can be transferred on-demand." A syncing replica can even prepare, commit and become primary meanwhile.

**Only fetch what changed.** `sync_enqueue_tables` (`replica.zig:10778–10830`) enqueues a table only if its `snapshot_min` (the op that created it) falls inside the skipped op range. Tables older than that are already on the local disk, byte for byte, because storage is deterministic (§3.9). So sync costs O(what changed while you were away), not O(database size).

**Why wait a checkpoint before clearing `sync_op_*`?** `sync.md` "5: Conclusion" gives a 7-step scenario. A table `t` that hasn't been synced yet gets released by compaction. Then the replica crashes, replays, and finds `t` missing, "despite having completed sync and having no storage corruption". Keeping the range until the next checkpoint means the replica keeps repairing until all such blocks are durable.

### 3.8 The checkpoint identifier

`checkpoint_id` is `vsr.checksum(std.mem.asBytes(&superblock.vsr_state.checkpoint))` (`superblock.zig:444–446`), a checksum of the 1024-byte `CheckpointState`. That struct holds the checksums of the manifest, free set and client-sessions roots (`superblock.zig:320–411`). Through the hash chain those roots cover every live block. So equal ids mean equal state.

It travels on `ping`, `commit`, `prepare` and `prepare_ok` (`sync.md`, "Checkpoint Identifier"). On a prepare, it is the id of the checkpoint the prepare was created in. Its purpose, per `message_header.zig:717–725`: "to strictly bound the number of commits that it may take to discover a divergent replica."

> ⚠️ **Docs vs code (wording).** `sync.md` says a checkpoint id "is a hash over the entire state". The code hashes only the 1024-byte `CheckpointState`. It covers the entire state *transitively* through the hash-chained block checksums it contains. Same guarantee, different mechanism; the code wins on the details.

### 3.9 Storage determinism: the precondition for all of this

Physical repair only works if every replica has the same bytes at the same address. From `ARCHITECTURE.md`, "Determinism": "It is guaranteed that replicas in the cluster converge on a byte-for-byte identical LSM tree structure", and "Physical determinism requires that LSM compaction work is scheduled deterministically." (Concept 20 covers this in depth.)

When it breaks, TigerBeetle stops. `on_prepare` panics with `"checkpoint diverged"` if a prepare's `checkpoint_id` matches neither our current nor our parent checkpoint. Per the comment, "it is not possible to distinguish whether the problem is with this replica, the prepare's replica, or both" (`replica.zig:2173–2193`). `sync.md`: "This scenario should prompt operator investigation and manual intervention."

> ⚠️ **Docs gap.** `vsr.md` "Protocol: Repair Grid" ends with "TODO Describe state sync fallback." The fallback is in the code: `repair_stuck` → `get_view` → `on_view_set_checkpoint` (§3.7).

## 4. What it means for ashi

**Determinism first, or none of this works.** ashi needs byte-identical storage before it can have block-level repair.
- Rust's `HashMap` uses `RandomState`, which randomizes the hash per process, so iteration order differs between replicas. Never let `HashMap` iteration order reach anything persisted or replicated. Use `BTreeMap`, sorted `Vec`s, or a fixed-seed hasher.
- Make every comparator total: break ties explicitly, as TigerBeetle's Marzullo sort does by source index (Concept 26). Then any correct sort algorithm gives the same order.
- Free-set allocation, compaction scheduling and block layout must be pure functions of the op stream. Nothing may depend on wall time, thread scheduling or I/O completion order.

**Patterns to copy:**
- *Block = message.* Give blocks the same 256-byte header as network messages (`#[repr(C, align(16))]`), and keep buffers `#[repr(align(4096))]`. Serving a repair is then one copy into a message buffer, with no serialization.
- *A read API with no error path* (`from_local_or_global_storage`). In Rust this is a callback or state-machine continuation that fires only with a valid block. That keeps `Result` plumbing out of the LSM. Avoid `Box<dyn Future>` per read (Concept 12): use a fixed pool of read slots.
- *Budgets as fixed-capacity maps.* TigerBeetle pre-sizes its `AutoArrayHashMapUnmanaged`s at init. In Rust: `heapless::FnvIndexMap`, `arrayvec`, or a hand-rolled array of `(key, Instant)`.
- *Lazy sync with a skipped-op range persisted in the superblock*, cleared only at the next checkpoint.

**Crates worth evaluating:** none are essential (TigerBeetle has zero dependencies). `heapless`/`arrayvec` for bounded collections. `blake3` is *not* a drop-in for the checksum: TigerBeetle uses AEGIS-128L in MAC mode (`checksum.zig`), and the choice is permanent once data is on disk.

**Rust pitfalls:**
- `std::time::Instant` and `tokio::time` in repair timeouts make the simulator non-deterministic. Inject a `Time` trait, as TigerBeetle injects `Time`.
- Randomized replica selection must use a *seeded* PRNG owned by the replica (`rand::rngs::SmallRng::seed_from_u64`), never `thread_rng()`.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt.** Physical repair plus lazy, range-bounded state sync is the only way to keep repair cost proportional to the damage. It forces storage determinism from day one, which ashi needs for simulation testing anyway.

## 5. Drill questions

### Q1. Recall

Name the four repair request messages and what each one repairs. Which one does state sync reuse to fetch LSM tables?

> _Your answer:_

### Q2. Mechanism

Explain why TigerBeetle repairs WAL *headers* newest-to-oldest but repairs *prepares* oldest-to-newest. What makes it safe to accept a header from a replica you don't trust?

> _Your answer:_

### Q3. Back-of-envelope

(a) Compute `vsr_checkpoint_ops` from the default constants.
(b) A cluster runs at 1,000,000 transfers/s with full batches of 8,190 transfers per prepare. Roughly how many seconds of downtime push a replica more than two checkpoints behind, so that it must state sync?
(c) With the default scrubber cycle, what is the worst-case scrub read rate on a full 64 TiB data file?

> _Your answer:_

### Q4. What breaks if…

…TigerBeetle stored the *sending replica's index* in each grid block header, to make the repair budget more accurate?

> _Your answer:_

### Q5. ashi design (open-ended)

ashi could sync a lagging replica *logically* (replay accounts and transfers into its own LSM) instead of *physically* (copy blocks). What would you gain (e.g. freedom to change on-disk layout per replica, or to compact on a different schedule) and what would you lose? Would you ever mix the two?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** `get_headers` (WAL headers: gaps and chain breaks), `get_prepare` (missing or corrupt prepares), `get_reply` (corrupt cached client replies), `get_blocks` (corrupt or missing grid blocks). State sync reuses `get_blocks` for the free set, client sessions, manifest and table blocks ("Sync Forest"), and `get_reply` for replies. The trigger is `get_view` → `view`, which carries the new checkpoint.

**Q2.** Trust flows from the head. The primary's `view` message gives a trusted header for the newest op, and each header names its parent's checksum. So you can verify only *backwards*. A header from any replica is accepted only if it connects through the chain to `self.op` (`repair_header_would_connect_hash_chain`). A forged or stale header won't match the checksum its child expects. Prepares go forwards because commit is sequential, so the oldest missing prepare is what blocks progress. Also, older entries are the first to be overwritten as the ring wraps, so fetching them first improves the odds that a peer still has them.

**Q3.** (a) 1024 − 32 − 32·⌈16/32⌉ = 1024 − 32 − 32 = **960**. (b) 1,000,000 / 8,190 ≈ 122 prepares/s. Two checkpoints = 1,920 ops ≈ **16 s**. (One checkpoint is ≈ 7.9 s.) At full load, even a short restart can force state sync, which is why sync must be cheap and lazy. (c) 64 TiB / (180 × 86,400 s) ≈ **4.3 MiB/s**, matching the comment in `constants.zig`.

**Q4.** Replicas would write *different bytes* for the same logical block (the sender index differs), so the block checksum would differ. A block's checksum lives in its parent, so the parents would differ too, all the way up to the manifest and `CheckpointState`. Replicas would then compute different `checkpoint_id`s for the same op. Every prepare would trip the `"checkpoint diverged"` panic, and physical block repair would stop working: a peer's block would never match the checksum the requester expects. That's why `repair_budget.zig` refunds every replica the block was requested from instead.

**Q5.** Open-ended. Logical sync allows per-replica layout and compaction freedom and tolerates software differences between replicas. It costs O(database) transfer and CPU, gives slow catch-up (hours at TB scale), and loses the cheap single-block repair path, which relies on identical bytes. A mix is possible: physical repair as the default, logical rebuild as an operator-triggered last resort after a determinism bug.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
