# Concept 18: The LSM forest

**Module:** M3 Storage · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/internals/lsm.md` (all sections: Glossary, Tables, Compaction, Selection Policy, Move Table, Table Overlap, Snapshots, Manifest)
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Batching" (the bullet list) and "LSM"
- `../tigerbeetle/docs/internals/data_file.md`, the LSM part ("Concretely, the state of TigerBeetle is a collection (forest) of LSM trees" onward)
- `../tigerbeetle/src/config.zig:161-173` and `../tigerbeetle/src/constants.zig:600-720` (LSM constants)
- `../tigerbeetle/src/state_machine.zig:46-90` (`tree_ids`), `:291-345` (`Forest`), `:399-560` (the four grooves), `:4661-4768` (`tree_values_count`), `:1804-1906` (`get_scan_from_account_filter`)
- `../tigerbeetle/src/lsm/groove.zig`, `forest.zig`, `tree.zig`, `table.zig`, `schema.zig`, `table_memory.zig`, `compaction.zig`, `manifest.zig`, `manifest_level.zig`, `manifest_log.zig`, `unique_key.zig`, `composite_key.zig`
- `../tigerbeetle/src/lsm/scan_builder.zig`, `scan_tree.zig`, `scan_merge.zig`, `zig_zag_merge.zig`, `cache_map.zig`, `set_associative_cache.zig`
- `../tigerbeetle/src/vsr/free_set.zig:29-44`, `../tigerbeetle/src/vsr/grid.zig:159-174`, `../tigerbeetle/src/stdx/radix.zig:1-9`

## 1. The problem

A ledger keeps three kinds of state forever:

- **Transfers.** Billions of them. Each is written once and never changed.
- **Accounts.** Far fewer, but every transfer updates two of them, and a few hot accounts are updated constantly (Concept 1).
- **Indexes.** "All transfers debiting account X", "all transfers with `code = 7`". These must be kept up to date on every write.

The storage engine has to:

- absorb a write-heavy stream cheaply (up to ~8,190 transfers per prepare, Concept 7);
- find any object by `id` quickly, for idempotency checks and balance updates;
- answer range queries over the indexes;
- never stall. A pause of hundreds of milliseconds to "catch up" is a tail-latency disaster for payments;
- produce **byte-identical** data on every replica, so a corrupted block can be fetched from a peer by checksum (Concept 20, Concept 25).

The last two requirements are what make TigerBeetle's design unusual.

## 2. The obvious solution, and why it fails

### LSM basics, for someone who hasn't built one

A **log-structured merge tree (LSM)** never updates data in place. It works like this:

```
 writes ──► memtable (in RAM, sorted or sortable)
               │ full?
               ▼
           immutable memtable (frozen, being written out)
               │ flush: write one sorted file ("table")
               ▼
 level 0   [t][t][t]                      small
 level 1   [t][t][t][t][t][t][t][t]       ×growth factor
 level 2   [t]... (×growth factor again)  ...
               ▲
               └── compaction: merge a table from level N into the
                   overlapping tables of level N+1, write new tables,
                   drop the old ones
```

- A **table** (also called an SSTable or a "sorted run") is an immutable, sorted array of key/value pairs on disk.
- An update is a new version of the key, written higher up. A delete is a **tombstone**: a marker that says "this key is gone". Newer levels win over older ones.
- **Compaction** merges levels, drops overwritten versions and tombstones, and keeps each level bounded.

Three costs trade off against each other:

| Cost | Definition | What drives it |
|---|---|---|
| **Write amplification** | bytes written to disk ÷ bytes the user wrote | Each value is rewritten once per level it passes through, and each merge also rewrites the overlapping tables below. |
| **Read amplification** | tables (or blocks) probed per lookup | Up to one table per level, plus the in-memory tables. |
| **Space amplification** | bytes on disk ÷ live data | Stale versions and tombstones not yet compacted away. |

A larger **growth factor** (size ratio between levels) means fewer levels, so cheaper reads. But each compaction overlaps more tables in the next level, so writes cost more.

### Option A: a B-tree updated in place

- A 128-byte balance change dirties a whole page (often 4–16 KiB). That is 32–128× write amplification on random pages.
- In-place overwrites clash with TigerBeetle's copy-on-write grid and its checksums stored in parent blocks (Concept 15, Concept 17). Every page write would ripple up to the root.

### Option B: one off-the-shelf LSM with key prefixes

This is what most databases do: one big tree, with "tables" faked as key prefixes (`acct/…`, `xfer/…`, `idx_debit/…`).

- **Variable-size values.** The engine stores lengths, prefixes and per-entry metadata, and can't do fixed-stride binary search inside a block.
- **One-size-fits-all tuning.** Append-only transfers, constantly updated accounts and 32-byte index entries all share one table size, one compaction policy and one cache.
- **Background compaction threads.** These run "when needed". When writes outpace them, the engine applies a **write stall**: it blocks the foreground until compaction catches up. That is unbounded tail latency.
- **Nondeterminism.** Which files get compacted, and when, depends on thread timing. Two replicas fed the same log end up with different files. Physical repair from a peer (Concept 20) becomes impossible.

**Back-of-envelope.** With a growth factor of 8 and 7 levels, a leveled LSM in the worst case rewrites each value roughly once per level, plus up to 8 overlapping tables per merge. That puts worst-case write amplification in the tens (my estimate, not from the source). If that work happens in bursts, a commit that lands during a burst waits for it.

## 3. What TigerBeetle does

The idea in one sentence: **many small, type-specialized LSM trees (a "forest"), whose compaction is chopped into equal slices that run after every commit, in a fully deterministic order.**

### 3.1 The constants

From `src/config.zig` (cluster config, production defaults) and `src/constants.zig`:

| Constant | Value | Source |
|---|---|---|
| `block_size` | `512 * KiB` | `config.zig:161` |
| `lsm_levels` | `7` | `config.zig:162` |
| `lsm_growth_factor` | `8` | `config.zig:163` |
| `lsm_compaction_ops` (beats per bar) | `32` | `config.zig:164` |
| `lsm_snapshots_max` | `32` (persistent snapshots are not implemented yet) | `config.zig:165` |
| `lsm_manifest_compact_extra_blocks` | `1` | `config.zig:166` |
| `lsm_table_coalescing_threshold_percent` | `50` | `config.zig:167` |
| `lsm_scans_max` | `6` | `config.zig:173` |
| `lsm_manifest_node_size` | `16 * KiB` | `config.zig:105` |
| `lsm_compaction_queue_read_max` / `_write_max` | `16` / `16` | `constants.zig:667-668` |
| tables per level `i` | `pow(growth_factor, i+1)` | `constants.zig:610`, `tree.zig:656-660` |

Why growth factor 8 and not the usual 10? The comment at `constants.zig:610-615` says it directly: "Since read amplification can be optimized more easily (with caching), we target a growth factor of 8 for lower write amplification rather than the more typical growth factor of 10."

Tables per level, from the test at `tree.zig:663-680`:

```
level:      0    1     2      3       4        5          6
tables:     8   64   512   4096   32768   262144   2097152     total per tree: 2,396,744
```

The last level holds 2,097,152 / 2,396,744 ≈ 87.5% of all tables. Most data lives at the bottom, so space amplification stays low.

### 3.2 From one tree to a forest

Three nouns (glossary in `lsm.md`):

- **Tree:** one LSM tree storing one fixed-size value type (`tree.zig:39`).
- **Groove:** the set of trees for one object type: one object tree plus its index trees (`groove.zig:138`).
- **Forest:** all the grooves (`forest.zig`).

A groove is **generated at compile time from the object's struct fields.** `GrooveType` walks `std.meta.fields(Object)` and creates one index tree per field, skipping `timestamp` and anything listed in `ignored` (`groove.zig:306-331`). Derived indexes add more trees (`groove.zig:333-376`). A comptime assertion checks the count (`groove.zig:406-414`).

There are three kinds of trees in a groove:

| Kind | Key | Value | Size | Built by |
|---|---|---|---|---|
| **Object tree** | `timestamp` (u64) | the whole object | 128 B (Account, Transfer), 256 B (AccountEvent), 16 B (TransferPending) | `groove.zig:385-399` |
| **Unique-key tree** ("id tree") | the field (e.g. `id`) | `{field, timestamp, padding}` | 32 B for a u128 field | `UniqueKeyTreeType`, `groove.zig:51-70`; layout `unique_key.zig:13-58` |
| **Secondary index tree** | `(field << 64) \| timestamp` | `{field, timestamp, padding}` | 8 B (void), 16 B (≤u64), 32 B (u128) | `IndexTreeType`, `groove.zig:73-90`; layout `composite_key.zig:12-75` |

The object tree is keyed by **timestamp, not id** (Concept 19 covers why). The object tree's key function strips a tombstone bit stolen from the top of the timestamp:

```zig
// src/lsm/groove.zig:29-35
inline fn key_from_value(value: *const Object) u64 {
    return value.timestamp & ~@as(u64, tombstone_bit);
}

const sentinel_key = std.math.maxInt(u64);
const tombstone_bit = 1 << (64 - 1);
```

The index trees store only `(field, timestamp)`. To get the object, you take the timestamp and look it up in the object tree.

**The actual forest.** `state_machine.zig:291-296` declares four grooves. Tree IDs come from `tree_ids` (`state_machine.zig:46-90`). Ignored fields come from each groove's config (`:399-560`).

```
Forest (33 trees, ids 1..33, no gaps — asserted at forest.zig:111-112)
│
├── accounts            (9 trees)
│   ├── object tree:   timestamp=7                    value = Account (128 B)
│   ├── unique:        id=1                           (32 B)
│   ├── index:         user_data_128=2, user_data_64=3, user_data_32=4, ledger=5, code=6
│   └── derived:       imported=23, closed=25         (void prefix, 8 B)
│       not indexed:   debits_posted, debits_pending, credits_posted, credits_pending, flags, reserved
│
├── transfers           (14 trees)
│   ├── object tree:   timestamp=18                   value = Transfer (128 B)
│   ├── unique:        id=8
│   ├── index:         debit_account_id=9, credit_account_id=10, amount=11, pending_id=12,
│   │                  user_data_128=13, user_data_64=14, user_data_32=15, ledger=16, code=17
│   └── derived:       expires_at=19, imported=24, closing=26
│       not indexed:   timeout, flags
│
├── transfers_pending   (2 trees)
│   ├── object tree:   timestamp=20                   value = TransferPending (16 B)
│   └── index:         status=21
│
└── account_events      (8 trees)
    ├── object tree:   timestamp=22                   value = AccountEvent (256 B)
    ├── index:         transfer_pending_status=28
    └── derived:       account_timestamp=27, dr_account_id_expired=29, cr_account_id_expired=30,
                       transfer_pending_id_expired=31, ledger_expired=32, prunable=33
```

The tree counts are my tally from the configs. The comptime asserts at `state_machine.zig:299-342` pin the number of grooves (4) and derived indexes per groove (2, 3, 0, 6).

Why a forest instead of one tree with key prefixes?

- **Fixed-size values per tree.** Every block holds a plain array of `Value`. Binary search uses a fixed stride, with no per-entry headers. `ARCHITECTURE.md` ("LSM"): "Each tree can be specialized for specific value size. This improves performance and storage efficiency."
- **Per-tree sizing.** A tree's table capacity is `lsm_compaction_ops × batch_value_count_max` for that tree (`groove.zig:322-323`, `:386-387`). Account objects get twice the transfer count, because each transfer updates two accounts (`state_machine.zig:4732-4735`).
- **Per-tree behaviour.** Secondary index tables use `TableUsage.secondary_index`, which lets a put and a matching tombstone cancel immediately in memory (`table.zig:16-30`, `table_memory.zig:432-449`).
- **Independent compaction.** The append-only transfer tree compacts almost for free (move-table, §3.6). The update-heavy account tree does real merges. One doesn't slow the other's policy.

### 3.3 Tables: an index block plus value blocks

A table is exactly one **index block** plus 1..N **value blocks**, all `block_size` (512 KiB) grid blocks (`table.zig:41-44`). The layout comment in `schema.zig:7-16`:

```
Index block body:                                Value block body:
│ [value_block_count_max]u256 │ checksums        │ [≤value_count_max]Value │ sorted values
│ [value_block_count_max]Key  │ min key per block│ […]u8{0}               │ padding
│ [value_block_count_max]Key  │ max key per block
│ [value_block_count_max]u64  │ addresses
│ […]u8{0}                    │ padding
```

- Each block starts with a 256-byte `vsr.Header`. The block's own checksum is in the header. The children's checksums are in the parent (Concept 15).
- The index block header's metadata records `value_block_count`, `value_block_count_max`, `key_size` and `tree_id` (`schema.zig:88-100`). The comment explains why: "Tables can be decoded without per-tree specialized decoders. (In particular, this is useful for the scrubber and the grid repair queue)."
- **Lookup inside a table:** binary-search the index block's key ranges to pick a value block, then binary-search that value block (`data_file.md`).

**Worked numbers for the transfers object tree.** Derived from the constants above, assuming 8,190 transfers per prepare (the figure in the comment at `constants.zig:647`):

```
block body          = 524,288 − 256          = 524,032 bytes
values per block    = 524,032 / 128          = 4,094 transfers
table capacity      = 32 ops × 8,190         = 262,080 transfers   (constants.zig:647-649)
value blocks        = ceil(262,080 / 4,094)  = 65
table on disk       = (65 + 1) × 0.5 MiB     ≈ 33 MiB
```

A transfers `id` tree (32-byte values) packs 16,376 values per block, so 17 value blocks per table.

### 3.4 Mutable and immutable in-memory tables

Each tree has two `TableMemory` instances (`tree.zig:69-70`; implementation `table_memory.zig`):

- **Mutable table.** Every `put` and `remove` goes here (`tree.zig:199-205`). Removes are tombstones. It is sized for a whole bar of updates (`lsm.md`, "Tables").
- **Immutable table.** A frozen, read-only staging area that gets flushed to level 0.

The module comment at `table_memory.zig:1-27` describes the current mechanism:

1. `put` just appends (`table_memory.zig:576-593`). If the value's key happens to be larger than the previous one, the current sorted run grows. So appending keys in order is free.
2. **After every beat**, the new suffix is radix-sorted and de-duplicated into a new sorted run (`tree.compact` → `sort_suffix`, `tree.zig:554-567`). The comment: "Spreads sort+deduplication work between beats, to avoid a latency spike at the end of each bar."
3. The sort is a stable LSD radix sort (`stdx/radix.zig:1`). Stability matters: de-duplication keeps the **last** write for a key (`table_memory.zig:446-448`).
4. **At the bar boundary** (`swap_mutable_and_immutable`, `tree.zig:570-605`):
   - if the old immutable table was flushed, the buffers are **swapped** (`table_memory.zig:690-713`);
   - otherwise the mutable runs are **absorbed** into the still-unflushed immutable table (`table_memory.zig:716`).
5. Lookups search runs newest-first (`table_memory.zig:596-627`). Up to `lsm_compaction_ops + 2` runs can exist (`table_memory.zig:91`).

**Deferred flush.** If the immutable table plus a worst-case half-bar of new values still fits in one table, `half_bar_commence` skips flushing it to level 0 (`compaction.zig:591-627`). This avoids writing many tiny tables when load is light. There is one exception: the last half-bar before a checkpoint trigger, "since recovery from the checkpoint only replays that final bar, which must reconstruct the original immutable table" (`compaction.zig:594-596`).

### 3.5 Beats and bars: paced compaction

This is the heart of the design. The vocabulary is musical:

- **beat** = one op's slice of compaction work: `beat = op % lsm_compaction_ops`.
- **bar** = `lsm_compaction_ops` beats = 32 ops.
- **half-bar** = 16 beats.

After every commit, the replica calls `state_machine.compact` (`vsr/replica.zig:4969`), which calls `forest.compact(callback, op)` (`state_machine.zig:2897`, `forest.zig:384`). The commit stages run in order: `prefetch → reply_setup → execute → checkpoint_durable → compact → checkpoint…` (`vsr/replica.zig:4427-4526`). The client's reply is sent during `execute` (`vsr/replica.zig:5549-5560`), *before* compaction. But the next op cannot start committing until this beat's compaction finishes. So a slow beat delays every request queued behind it (Concept 10 covers the commit pipeline).

**Which levels run when.** Each tree has one `Compaction` per target level `level_b` (`tree.zig:78`). Even and odd target levels alternate half-bars:

```zig
// src/lsm/forest.zig:1093-1097
fn level_active(options: struct { level_b: usize, op: u64 }) bool {
    const half_bar_beat_count = @divExact(constants.lsm_compaction_ops, 2);
    const compaction_beat = options.op % constants.lsm_compaction_ops;
    return (compaction_beat < half_bar_beat_count) == (options.level_b % 2 == 1);
}
```

```
beat:     0 ............................ 15 | 16 ............................ 31
          first half-bar                     | second half-bar
active:   0→1, 2→3, 4→5                      | immutable→0, 1→2, 3→4, 5→6
          (level_b odd)                      | (level_b even)
events:   beat 0: plan half-bar quotas       | beat 16: plan half-bar quotas
          each beat: beat quotas + reserve   | each beat: beat quotas + reserve
          beat 15: apply manifest updates,   | beat 31: apply manifest updates,
                   compact manifest log      |   compact manifest log, swap mutable→immutable,
                                             |   flush storage (flush_sectors)
```

Why alternate? A table compacted from level 1 to 2 is never being written to level 1 by a 0→1 compaction at the same moment. Each level is either a source or a target in any half-bar, never both. With 7 levels, at most ⌈7/2⌉ = 4 compactions run per tree at once (`lsm.md`).

**Pacing with quotas.** At the first beat of each half-bar, the forest asks every active compaction how many input values it will consume (`half_bar_commence`, `compaction.zig:565`). It sums them **in bytes** across all trees. Then each beat gets an equal share of what remains:

```zig
// src/lsm/forest.zig:505-512
const beats_total = half_bar;
const beats_done = compaction_beat % half_bar;
const beats_remaining = beats_total - beats_done;

forest.progress.?.compact.trees_beat_input_size = stdx.div_ceil(
    forest.compact_trees_half_bar_input_size,
    beats_remaining,
);
```

Each compaction's `quotas` (`compaction.zig:376-392`) track `beat`, `beat_done`, `half_bar` and `half_bar_done`. The comment says: "Quotas track logical progress of compaction, determine pacing and must be deterministic."

The beat's byte budget is then handed out to the active compactions **in a fixed order** (level, then tree id). Each takes `min(budget, its remaining half-bar work)` (`beat_commence`, `compaction.zig:903-920`). The forest then reserves an upper bound of grid blocks for the beat's output (`compact_trees_reserve_grid_blocks`, `forest.zig:521-569`).

**Worked example.** At beat 16, the second half-bar's compactions across all trees need to read 160 MiB of input. Each of the 16 beats then does ~10 MiB of merging. Without pacing, one op's compaction step would do all 160 MiB, and every request queued behind it would wait.

Invariants from `lsm.md` ("Compaction"):
- At the end of every beat, the mutable table has space for the next beat.
- The manifest log is compacted every half-bar.
- Output tables are invisible until their compaction finishes.

### 3.6 Selection, move-table and overlap

**When a level compacts.** `Manifest.compaction_table` (`manifest.zig:532-564`) returns null unless level A has reached its table limit:

```zig
// src/lsm/manifest.zig:542-544
// If even levels are compacted ahead of odd levels, then odd levels may burst.
assert(manifest_level_a.table_count_visible <= table_count_visible_max + 1);
if (manifest_level_a.table_count_visible < table_count_visible_max) return null;
```

**Which table.** The policy is "least overlapping with parent": pick the level-A table that overlaps the fewest level-B tables (`lsm.md`, "Compaction Selection Policy"). `table_with_least_overlap` (`manifest_level.zig:584`) does it with a two-pointer sweep in O(n_a + n_b).

**Overlap bound.** At most `lsm_growth_factor` level-B tables can overlap the chosen table. Tables within a level are disjoint, and B has at most 8× as many tables as A. If every A table overlapped more than 8, the B tables would not fit. So some A table overlaps 8 or fewer, and that is the one chosen. The code relies on this bound:

```zig
// src/lsm/compaction.zig:57-61
/// The upper-bound count of input tables to a single tree's compaction.
/// - +1 from level A.
/// - +lsm_growth_factor from level B. ...
pub const compaction_tables_input_max = 1 + constants.lsm_growth_factor;
```

That bound is what makes static allocation of compaction buffers possible (Concept 11).

**Move-table.** If the chosen on-disk table overlaps **nothing** in level B, it is not rewritten. Only its manifest entry changes:

```zig
// src/lsm/compaction.zig:694-695
compaction.move_table = compaction.table_info_a.? == .disk and
    compaction.range_b.?.tables.empty();
```

`lsm.md`: "Where a tree performs inserts mostly in sort order, with a minimum of updates, this move table optimization should enable the tree's performance to approach that of an append-only log." The transfers object tree is keyed by a strictly increasing timestamp, so it is exactly this case (Concept 19). Note that `.immutable` → level 0 is never a move: the in-memory table must be written once.

**Merging.** Otherwise, sort-merge table A with the concatenation of tables B. On equal keys, A (newer) wins (`compaction.zig:17-18`). **Tombstones are dropped** when B is the last level, or when no deeper level contains the key range (`compaction_must_drop_tombstones`, `manifest.zig:637-664`).

### 3.7 The manifest and the manifest log

The **manifest** is each tree's index of its tables: which table is at which level, with what key range and snapshots.

- **In memory:** one `ManifestLevel` per level, backed by sorted segmented arrays (an "unrolled linked list", `segmented_array.zig`). Their nodes come from a `NodePool` of 16 KiB nodes (`groove.zig:17`). The default manifest memory is 8,192 nodes × 16 KiB = 128 MiB (`constants.zig:689-693`).
- **Each entry** is a 128-byte `TableInfo` (`schema.zig:557-576`): padded `key_min` and `key_max` (32 B each), the index block's checksum and address, `snapshot_min`, `snapshot_max`, `value_count`, `tree_id`, and a one-byte `Label` of `{level: u6, event: insert|update|remove}`.
- **On disk:** one **manifest log** shared by all trees (`manifest_log.zig`). It is an append-only log of `TableInfo` events, stored in grid blocks. Each block points back to the previous one. The superblock stores the head and tail (`lsm.md`, "Manifest Log"). One 512 KiB block holds 524,032 / 128 = 4,094 entries.
- **Opening** replays the log to rebuild every tree's levels (`manifest_log.zig:256`, `forest.zig:345`).
- **Compaction of the log itself.** Superseded entries (for example, a table later removed) are rewritten away a few blocks per half-bar. `ManifestLog.Pace` (`manifest_log.zig:1076`) bounds the log's size. `lsm_manifest_compact_extra_blocks = 1` makes compaction outrun appends (`constants.zig:627-639`).

Manifest updates are **queued and applied at the end of the half-bar**, not as each compaction finishes:

```zig
// src/lsm/compaction.zig:831-834
// Each compaction's manifest updates are deferred to the end of the last
// bar to ensure:
// - manifest log updates are ordered deterministically relative to one another, and
// - manifest updates are not visible until after the blocks are all on disk.
```

### 3.8 Snapshots

Each table has `snapshot_min` and `snapshot_max`. A table is visible to snapshot `S` when `snapshot_min ≤ S ≤ snapshot_max` (`lsm.md`, "Snapshots"). Snapshots are op numbers.

For a half-bar that starts at `op_min` (`compaction_op_min`, `compaction.zig:2375-2378`):

```zig
// src/lsm/compaction.zig:2300-2308
pub fn snapshot_max_for_table_input(op_min: u64) u64 {
    return snapshot_min_for_table_output(op_min) - 1;
}

pub fn snapshot_min_for_table_output(op_min: u64) u64 {
    assert(op_min > 0);
    assert(op_min % @divExact(constants.lsm_compaction_ops, 2) == 0);
    return op_min + @divExact(constants.lsm_compaction_ops, 2);
}
```

```
op:      0   4   8  12  16  20  24          (lsm.md example, lsm_compaction_ops = 8)
         ┼───┬───┼───┬───┼───┬───┼
                     ####                    half-bar X = 12..15
input tables:   ·····────────────           snapshot_max truncated to 15
output tables:                   ────····   snapshot_min = 16, snapshot_max = ∞
```

- Queries during ops 12–15 still see the inputs. The half-written outputs are invisible.
- From op 16 on, queries see only the outputs. The inputs can be freed, because no persistent snapshot holds them.
- Current reads use `snapshot_latest = maxInt(u64) - 1` (`tree.zig:24`). It is one less than the "never deleted" `snapshot_max = maxInt(u64)`, so a query never exactly equals a table bound.
- **Persistent snapshots** (querying old states) are "unimplemented; future work" (`lsm.md`). `lsm_snapshots_max` exists in config but nothing else in `src/` references it.

Compaction copies and never modifies a table in place. Together with the grid's copy-on-write blocks (Concept 17), this is what lets a checkpoint point at a consistent set of tables.

### 3.9 Determinism of compaction

Physical repair needs every replica to write **the same bytes to the same grid address** (Concept 20). Then a replica with a corrupt block can ask a peer for "address A, checksum C" (Concept 25). TigerBeetle gets this by making every compaction decision a pure function of the committed op sequence:

- **When:** compaction is driven by `op`, never by a timer or a background thread.
- **What:** selection depends only on the manifest, which depends only on earlier ops.
- **How much per beat:** quotas count logical input values. The physical IO counters are explicitly "not fully deterministic" and do not drive anything (`compaction.zig:362-367`).
- **Where (block addresses):** compactions run concurrently and finish IO in any order. So each one first **reserves** blocks from the free set in a fixed order, then acquires only from its own reservation (`free_set.zig:29-44`, `compaction.zig:991-994`).
- **Manifest order:** updates are queued and applied in a fixed order at half-bar end (§3.7).
- **Buffer count doesn't matter:** "Compactions will be deterministic regardless of how much blocks you give them, but will run in fewer steps with more memory" (`forest.zig:173-175`).

### 3.10 Lookups and caches

A point lookup by primary key goes from cheap to expensive:

```
1. Groove objects cache (CacheMap)     ── hit? done, no IO
2. tree.key_range_contains(key)        ── outside [min,max] of the whole tree? not found, no IO
3. immutable table                     ── in-memory binary search over runs
4. lookup_from_levels_cache            ── walk levels 0..6 using only blocks already in the grid cache
5. lookup_from_levels_storage          ── real reads, starting at the level where step 4 gave up
```

- Step 4 is `tree.zig:253-297`. It returns `.possible = level` when a needed block isn't cached, so step 5 resumes from there.
- **There are no Bloom filters.** A grep of `src/lsm` finds the word only once, in a comment in `scan_tree.zig:833`. The negative-lookup shortcuts are the tree-wide key range, the in-memory objects cache, and the grid cache.

**`SetAssociativeCache`** (`set_associative_cache.zig:31`). Each key hashes to a set of `ways` (default 16) slots. Each slot has a small tag (default 8 bits) and a CLOCK counter (default 2 bits) (`set_associative_cache.zig:16-18`). Tags are packed so a set's tags share a cache line. Eviction is CLOCK "Nth chance". No pointers and no allocation after init, which fits Concept 9 and Concept 11. The grid's block cache is one of these, keyed by block address (`grid.zig:159-174`), with a 1 GiB default size (`config.zig:132`).

**`CacheMap`** (`cache_map.zig:11-25`) is a set-associative cache plus a hash map "stash". Values evicted from the cache land in the stash, so **values prefetched for this commit are guaranteed to stay in memory** until the stash is cleared at the end of the bar (`groove.zig:1954-1960`). The lookup order is "cache (if present) -> stash -> immutable table -> lsm" (`cache_map.zig:82`). It also supports scopes with a rollback log, which linked event chains use to undo a failed chain (`cache_map.zig:329-337`). Default sizes: 1 Mi accounts' worth (`@sizeOf(Account) * MiB`) for the accounts cache, and 0 for transfers (`config.zig:222-224`).

### 3.11 Scans and queries

Index trees make range queries possible. A composite key is `(field << 64) | timestamp` (`composite_key.zig:65-72`). So for a fixed field value (say `debit_account_id = X`), the matching entries are **contiguous and sorted by timestamp**.

- **`ScanTree`** (`scan_tree.zig:25-35`) iterates one tree over a key range at a snapshot. It sorts the in-memory tables and binary-searches them, buffers matching values from every level, and k-way merges them in ascending or descending order.
- Scans are **streams**: `next()` can return `error.Pending`, meaning "more IO is needed, call me back" (`scan_builder.zig:22-28`). No async runtime is involved (Concept 12).
- **Union (OR)** uses a k-way merge. **Intersection (AND)** uses a **zig-zag merge join** (`scan_merge.zig:173-177`, `zig_zag_merge.zig:11-15`). Zig-zag: peek each stream's next timestamp. If they differ, `probe` the lagging streams forward to the largest one, skipping whole ranges without reading every entry. Emit when all agree. **Difference** is declared but `unimplemented` (`scan_merge.zig:176`).
- Because every index is sorted by timestamp within a prefix, **any index from Transfers, TransfersPending and AccountEvents can be combined**, since related objects share a timestamp (`scan_builder.zig:31-36`).

A real query plan: `get_account_transfers` (`state_machine.zig:1835-1906`) builds

```
(debit_account_id = X  ∪  credit_account_id = X)      ← merge_union
  ∩ user_data_128 = ?  ∩ user_data_64 = ?  ∩ user_data_32 = ?  ∩ code = ?   ← merge_intersection
```

then loads each resulting timestamp from the transfers object tree (`scan_lookup.zig`).

### 3.12 ⚠️ Docs vs code

The code wins in each case:

- **Trigger threshold.** `lsm.md` ("Compaction Table Overlap") says compaction triggers when level A's count *exceeds* its maximum. The code triggers when the count *reaches* the maximum (`manifest.zig:544`), and allows overflow by one (`manifest.zig:543`).
- **Mutable-table sorting.** `lsm.md` step 4 says the last beat will "Flush, clear, and sort mutable table values into immutable table". In the code, sorting happens incrementally every beat (`tree.zig:554-567`). The immutable table's flush can also be **deferred**, with the mutable table **absorbed** into it (`compaction.zig:591-627`, `tree.zig:589-601`). `lsm.md` doesn't describe either.
- **Typos in a code comment.** The comment at `forest.zig:445-447` gives the examples "2 → 4" and "1 → 3". `level_active` (`forest.zig:1093-1097`) and its test actually yield 2→3 and 1→2.
- **Index block layout.** `data_file.md` sketches the index block as `u128` checksums with only `value_block_key_max`. The real layout (`schema.zig:7-12`) has 32-byte checksum slots (u128 + padding, `schema.zig:74-78`), both min and max keys, and addresses.
- **Reservation granularity.** `lsm.md` steps 1–4 describe acquiring Free Set reservations at the first beat of each half-bar and releasing them at its last beat. The code reserves **per beat**: `compact_trees_reserve_grid_blocks` runs every beat and asserts there is no prior reservation (`forest.zig:514`, `:521-569`).
- **Checkpoint interval.** `ARCHITECTURE.md` ("Batching") says "Changes from 1024 prepares are aggregated before the checkpoint". `vsr_checkpoint_ops = journal_slot_count - lsm_compaction_ops - lsm_compaction_ops * div_ceil(pipeline_prepare_queue_max * 2, lsm_compaction_ops)` = 1024 − 32 − 32 = **960** (`constants.zig:40-42`), which is 30 bars.
- **Typo in `lsm.md`.** "Snapshot Values" says updates "were moved to the immutable table from the immutable table". It means *from the mutable table*.

## 4. What it means for ashi

**Rust features that help:**
- **Generics plus traits with associated consts** are Rust's version of Zig's `TableType(Key, Value, key_from_value, …)`. Example: `trait TableValue: Copy { type Key: Ord + Copy; const SIZE: usize; fn key(&self) -> Self::Key; fn is_tombstone(&self) -> bool; }`. Monomorphization gives one specialized tree per type, as comptime does.
- **Compile-time layout checks.** `const { assert!(size_of::<Transfer>() == 128) }` (inline const, stable since 1.79) plays the role of Zig's `comptime { assert(...) }`.
- **`#[repr(C, align(16))]` value structs** with explicit padding fields, plus `#[repr(C, align(4096))] struct Block([u8; BLOCK_SIZE])` for Direct IO (Concept 14). Read a value block as `&[Transfer]` by zero-copy casting.
- **Enums for compaction stages** (`inactive | beat | paused`) give an explicit state machine with exhaustive matching (Concept 12).

**Where Rust hurts:**
- **Arrays sized by generic expressions.** `[V; BLOCK_BODY / size_of::<V>()]` inside a generic needs the unstable `generic_const_exprs`. Work around it: keep blocks as `[u8; N]` and compute counts as associated consts used only at runtime. Associated consts like `const PER_BLOCK: usize = BODY / size_of::<Self>();` are fine. Using them as array lengths in generic code is not.
- **Struct reflection.** Zig builds a groove's index trees by iterating struct fields at comptime. Rust needs either a derive proc-macro (`syn`/`quote`: build-time dependencies, though not runtime) or hand-written, macro_rules-generated tree lists. With only ~33 trees, hand-writing plus a `macro_rules!` may be the honest choice.
- **`u128` alignment.** Since Rust 1.77, `u128` is 16-byte aligned on x86-64; before that it was 8. Assert alignment explicitly. TigerBeetle itself works around 8-aligned u128 in `composite_key.zig:31-35`.
- **Hidden nondeterminism.** `std::collections::HashMap` defaults to a randomly seeded `RandomState`. Never let iteration order of such a map influence what is written (manifest order, block allocation). Use `BTreeMap`, a fixed hasher, or sorted vectors.
- **Stable sort for last-write-wins.** De-dup must keep the newest write, so the in-memory sort must be stable. `slice::sort_by_key` is stable; `sort_unstable_by_key` is not. A hand-rolled LSD radix sort, like `stdx/radix.zig`, is stable and fast for integer keys.

**Patterns to adopt:**
- **Single writer.** Compaction runs on the same thread as commit, sliced into beats. Use no `tokio` task or `Box<dyn Future>` per compaction. Use callbacks or a polled state machine, with all buffers preallocated (Concept 11).
- **Reserve-then-acquire block allocation** for deterministic addresses under concurrent IO.
- **Fixed-size `TableInfo`** (128 B) and append-only manifest log blocks.
- **Index entries as `(prefix, timestamp)` integers,** so range scans and zig-zag joins are integer comparisons.

**Crates worth evaluating (TigerBeetle has zero dependencies, Concept 13):**

These are from general knowledge, not from the TigerBeetle clone. Verify against current versions.

| Crate | Pros | Why it doesn't give TigerBeetle's properties |
|---|---|---|
| `rocksdb` (rust-rocksdb, C++) | Battle-tested, tunable | Background compaction threads and write stalls; SST files in a filesystem rather than a checksummed grid; compaction choices depend on timing, so replicas diverge physically; large C++ build dependency. |
| `fjall` (pure-Rust LSM) | Rust-native, multiple keyspaces (similar to a forest) | Background flush/compaction workers; variable-size values; no byte-identical guarantee across replicas. |
| `sled` | Pure Rust | Long-running beta with an in-progress rewrite; not designed for deterministic layout. |
| `zerocopy` / `bytemuck` | Sound zero-copy casts of blocks to `&[Value]` | Small and safe. Worth it unless ashi wants zero deps; then write a narrow `unsafe` cast with asserted size and alignment. |

The core issue: an off-the-shelf engine can store ashi's data, but **cannot give physical determinism** (Concept 20) **or beat-paced latency.** Using one for a prototype is fine. Using one for production means giving up physical repair.

**Pitfalls specific to Rust:**
- `Vec` growth inside a mutable table breaks static allocation. Preallocate with `Vec::with_capacity` once and assert `len < capacity` before each push.
- Borrowing a `&[Value]` out of a block buffer while compaction reuses that buffer is what the borrow checker exists to stop. Design buffer ownership (a block pool with explicit stages, like `ResourcePool.Block.stage` in `compaction.zig:128-151`) so lifetimes are simple.
- `async` cancellation can drop a half-done compaction mid-beat. Explicit state machines can't be dropped silently.

**ashi stance:** _to be decided once the drill is done_

Suggestion: **adopt** the forest, fixed-size trees and beat-paced deterministic compaction, because physical repair and bounded latency depend on them. **Adapt** the comptime groove generation into Rust generics plus a small macro.

## 5. Drill questions

### Q1. Recall

Name the three kinds of trees in a groove and what each stores as key and value. How many trees does TigerBeetle's `transfers` groove have, and which `Transfer` fields get no index?

> _Your answer:_

### Q2. Mechanism

Explain why TigerBeetle splits compaction into beats with quotas, and alternates even and odd target levels between half-bars. What would happen to p99 commit latency if compaction instead ran all at once whenever a level filled up?

> _Your answer:_

### Q3. Back-of-envelope

Using `block_size = 512 KiB`, a 256-byte block header, 128-byte transfers and 262,080 transfers per table:
- How many value blocks does one transfers object table need, and how big is the table on disk?
- With 7 levels and growth factor 8, what is the maximum number of tables in one tree?
- Roughly how many transfers could the transfers object tree hold if every table were full?
- What other limit would stop you first?

> _Your answer:_

### Q4. What breaks if…

Suppose ashi runs compaction on a background thread pool. Each compaction allocates grid blocks from the free set as its IO completes. Logically, every replica ends up with the same key/value contents. What concretely breaks, and which later concepts depend on it?

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle generates index trees from struct fields at comptime. For ashi, choose between:
- (a) a derive proc-macro,
- (b) hand-written trees with `macro_rules!`,
- (c) a runtime schema (trees described by data, not types).

What do you gain and lose in performance, static allocation, compile times, dependencies and the ability to add new object types later?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- **Object tree:** key is `timestamp` (u64, top bit used as tombstone flag); value is the whole object.
- **Unique-key tree** (e.g. `id`): key is the field; value is `{field, timestamp, padding}`. It maps id → timestamp.
- **Secondary index tree:** key is `(field << 64) | timestamp`; value is `{field, timestamp, padding}`. Optional fields skip zero values; derived indexes come from functions of the object.
- `transfers` has **14 trees** (ids 8–19, 24, 26):
  - the object tree;
  - the `id` unique tree;
  - 9 field indexes: `debit_account_id`, `credit_account_id`, `amount`, `pending_id`, `user_data_128`, `user_data_64`, `user_data_32`, `ledger`, `code`;
  - 3 derived indexes: `expires_at`, `imported`, `closing`.
- Fields with no index: `timeout` and `flags` (`state_machine.zig:448`). `timestamp` is the object key, not an index.

**Q2.**
- **Why pace it:** compaction work is proportional to data volume. Doing it in one burst means the op that triggers it, and every request queued behind it, waits for all of it (the reply for that op is already sent, but the commit pipeline is sequential). Spreading a half-bar's total input bytes evenly over its 16 beats (`forest.zig:505-512`) bounds each commit's extra work to about 1/16 of that half-bar's work. The tail looks like the median.
- **Why alternate:** in any half-bar, a level is either a source or a target, never both. No level is read by one compaction while being rewritten by another. Merges stay simple, and the concurrent compactions per tree are bounded (≤ 4 with 7 levels).
- **Without pacing:** p99 and p999 would show periodic spikes of the full compaction cost (potentially seconds on a big level). Or you would need RocksDB-style write stalls. That is unbounded latency for payments.

**Q3.**
- 524,288 − 256 = 524,032 body bytes ÷ 128 = **4,094 transfers per value block**. ceil(262,080 / 4,094) = **65 value blocks** + 1 index block = 66 blocks × 0.5 MiB ≈ **33 MiB per table**.
- Maximum tables per tree: 8 + 64 + 512 + 4,096 + 32,768 + 262,144 + 2,097,152 = **2,396,744** (`tree.zig:679`).
- Capacity: 2,396,744 × 262,080 ≈ **6.3 × 10¹¹ transfers**. That is ~80 TB of transfer objects alone at 128 B each.
- You'd hit the storage limit first: `storage_size_limit_max = 64 * TiB` (`config.zig:99`), default 16 TiB. Index trees, account events and the forest-wide table budget also count: `Forest.table_count_max` equals one tree's maximum, shared by all trees (`forest.zig:24-28`).

**Q4.**
- Different replicas would put the same logical table at **different grid addresses**, possibly with different table boundaries. So the block at address A has a different checksum on each replica.
- Then:
  - **grid repair** (Concept 25) can't fetch "address A, checksum C" from a peer;
  - **state sync** can't copy a peer's checkpoint block-for-block;
  - checkpoint identifiers, which hash the superblock and manifest/free-set references, differ between replicas, so you can't cross-check replicas for silent divergence;
  - the **VOPR** (Concept 29) can no longer replay a seed and get a byte-identical result.
- Fix: drive compaction by `op`, reserve blocks in a fixed order, and apply manifest updates in a fixed order (§3.9).

**Q5.** There is no single right answer. Points a good answer covers:
- **(a) Derive macro:** closest to Zig ergonomics, and fully static, monomorphized trees. Costs: `syn`/`quote` build dependencies, slower compiles, and macro code that is hard to debug.
- **(b) `macro_rules!` + hand-written:** zero dependencies and explicit. With ~33 trees it's manageable, but adding a field means touching several places. Add compile-time asserts on counts, as TigerBeetle does at `state_machine.zig:299-342`.
- **(c) Runtime schema:** flexible (user-defined objects?), but values become byte slices with runtime sizes. You lose fixed-stride specialization and some compile-time checks, and static allocation must be computed at startup from the schema.
- A common middle path: generic `Tree<V: TableValue>` + `macro_rules!` grooves for v1, revisiting a derive macro if the object count grows.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
