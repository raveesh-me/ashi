# Concept 16: Data file layout

**Module:** M3 Storage · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/internals/data_file.md` (whole document)
- `../tigerbeetle/docs/internals/vsr.md`, section "Glossary"
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Overview" and "Hash-Chaining"
- `../tigerbeetle/src/vsr.zig` (`Zone`, `BlockReference`)
- `../tigerbeetle/src/constants.zig` (`journal_size`, `client_replies_size`, `message_size_max`, `superblock_copies`, `block_size`)
- `../tigerbeetle/src/config.zig` (`ConfigCluster`, `default_production`)
- `../tigerbeetle/src/vsr/superblock.zig` (`SuperBlockHeader`, `superblock_zone_size`, `data_file_size_min`, write/read paths)
- `../tigerbeetle/src/vsr/superblock_quorums.zig` (`Threshold`, `working`, `count_copy`)
- `../tigerbeetle/src/vsr/journal.zig` (`Ring`, recovery decision table, `write_prepare`)
- `../tigerbeetle/src/vsr/client_replies.zig` (module comment)
- `../tigerbeetle/src/storage.zig` (`start_write`)
- `../tigerbeetle/src/stdx/io/linux.zig` (`open_data_file`)

## 1. The problem

A replica must persist several different kinds of state:

- the **write-ahead log (WAL)**: recent prepares, the source of truth until they are checkpointed,
- the **LSM forest**: accounts, transfers and indexes, potentially terabytes,
- the **latest reply** to each client, so a retrying client gets the same answer (Concept 27),
- a small **root record** saying where everything else is.

Each has a different write pattern and a different failure story. The WAL is append-heavy and must survive torn writes. The forest is bulk data written in large blocks. The root must be updated atomically.

The usual approach is one file per concern: a WAL directory with segment files, SSTable files for the LSM, a `MANIFEST` file, a `CURRENT` file. That puts the file system in charge of allocation, naming, renames and directory durability. Concept 14 showed why TigerBeetle doesn't trust that layer.

## 2. The obvious solution, and why it fails

The RocksDB-style layout:

```
db/
  000123.log         WAL segment
  000045.sst ...     thousands of table files
  MANIFEST-000007    log of table additions and removals
  CURRENT            text file naming the live MANIFEST
```

What goes wrong for a ledger:

- **Many files mean many metadata operations.** Creating, renaming and deleting files changes directory entries. Making those durable needs `fsync` on the directory too, and the rules differ between file systems. A rename that "succeeded" can vanish after a crash.
- **Space can run out mid-operation.** An append can fail with `ENOSPC` at any time.
- **You need a file system.** You cannot run on a raw block device.
- **Physical layout is not deterministic.** Two replicas with the same logical data have different file names, sizes and on-disk positions. Repair must be logical ("send me table X"), not physical ("send me block 4711").
- **The root is a single file.** If `CURRENT` or `MANIFEST` is corrupt, the database won't open.

## 3. What TigerBeetle does

> "Each TigerBeetle replica stores all data inside a single file, called the data file (conventional extension is `.tigerbeetle`)." (`data_file.md`)

The file is divided into fixed **zones**, laid out back to back. The zones are an enum, and their order *is* the on-disk order (`src/vsr.zig:129-139`):

```zig
pub const Zone = enum {
    superblock,
    wal_headers,
    wal_prepares,
    client_replies,
    // Add padding between `client_replies` and `grid`, to make sure grid blocks are aligned to
    // block size and not just to sector size. ...
    grid_padding,
    grid,
    ...
```

A zone's start offset is the sum of the sizes of all zones before it, computed at compile time (`vsr.zig:183-190`). Every zone but the grid has a fixed size. The grid is last, so it can grow.

### 3.1 Sizes, with production values

The production config is `configs.default_production` (`src/config.zig:219-230`), which sets `clients_max = 64` and takes the `ConfigCluster` defaults (`config.zig:152-168`): `journal_slot_count = 1024`, `message_size_max = 1 * MiB`, `superblock_copies = 4`, `block_size = 512 * KiB`. `sector_size = 4096` (`constants.zig:493-496`). A `Header` is 256 bytes (`message_header.zig:72`).

| Zone | Formula (source) | Production size |
|---|---|---|
| `superblock` | `superblock_copy_size * superblock_copies` (`superblock.zig:602`) | 4 × 24 KiB = **96 KiB** |
| `wal_headers` | `journal_slot_count * @sizeOf(vsr.Header)` (`constants.zig:197`) | 1024 × 256 B = **256 KiB** |
| `wal_prepares` | `journal_slot_count * message_size_max` (`constants.zig:198`) | 1024 × 1 MiB = **1 GiB** |
| `client_replies` | `clients_max * message_size_max` (`constants.zig:178`) | 64 × 1 MiB = **64 MiB** |
| `grid_padding` | align the grid start up to `block_size` (`vsr.zig:145-156`) | **160 KiB** |
| `grid` | unbounded (`size()` returns `null`) | up to `storage_size_limit_max` = 64 TiB (`config.zig:99`) |

How the superblock copy size comes out at 24 KiB:

- `SuperBlockHeader` puts its fixed fields in the first sector (asserted: `@offsetOf(SuperBlockHeader, "view_headers_all") == constants.sector_size`, `superblock.zig:105`).
- Then `view_headers_max` prepare headers. `view_headers_max = view_change_headers_suffix_max + 2` (`constants.zig:304`) = 9 + 2 = 11, so 11 × 256 B = 2,816 B, padded to the next sector. Total header: 8 KiB (2 sectors).
- Then `superblock_copy_padding`, reserved room in case `pipeline_prepare_queue_max` is raised later (`superblock.zig:604-610`): `div_ceil((64 − 8) × 256, 4096) × 4096` = 16 KiB.
- 8 KiB + 16 KiB = 24 KiB per copy.

These byte values are my hand calculation from the constants. The code computes them at comptime and asserts sector alignment, but does not spell out the totals. A cross-check: `linux.zig:1705-1706` reads the superblock zone into a 128 KiB stack buffer and asserts it fits, consistent with 96 KiB.

`data_file_size_min` is everything before the grid (`superblock.zig:620-624`). With the numbers above it is 1 GiB + 64 MiB + 512 KiB = **1,141,374,976 bytes**. That is the file size right after `format`, with an empty grid.

### 3.2 The full layout

```
offset (bytes)                                                  zone
0             ┌──────────────────────────────────────────────┐
              │ superblock copy 0   8 KiB header + 16 KiB pad │  superblock
24,576        │ superblock copy 1                             │  (96 KiB)
49,152        │ superblock copy 2                             │
73,728        │ superblock copy 3                             │
98,304        ├──────────────────────────────────────────────┤
              │ header slot 0 │ slot 1 │ ... │ slot 15        │  wal_headers
              │ (16 × 256 B = one 4 KiB sector)               │  (256 KiB,
              │ ...                                           │   64 sectors)
              │ ... slot 1023                                 │
360,448       ├──────────────────────────────────────────────┤
              │ prepare slot 0      (1 MiB)                   │  wal_prepares
              │ prepare slot 1      (1 MiB)                   │  (1 GiB)
              │ ...                                           │
              │ prepare slot 1023   (1 MiB)                   │
1,074,102,272 ├──────────────────────────────────────────────┤
              │ reply slot 0        (1 MiB)                   │  client_replies
              │ ...                                           │  (64 MiB)
              │ reply slot 63       (1 MiB)                   │
1,141,211,136 ├──────────────────────────────────────────────┤
              │ zeroes, never read or written after format    │  grid_padding (160 KiB)
1,141,374,976 ├──────────────────────────────────────────────┤  ← multiple of 512 KiB
              │ block address 1     (512 KiB)                 │  grid
              │ block address 2     (512 KiB)                 │  (grows to
              │ block address 3 ...                           │   --limit-storage)
              ▼                                               ▼
```

Block address `a` lives at `grid_start + (a − 1) × block_size` (`grid.zig:1579-1583`). Address 0 is never used, so a zeroed field can't look like a valid address.

### 3.3 Why one file, preallocated

- **No metadata churn.** After format, TigerBeetle never creates, renames or deletes a file. Every write is an overwrite inside an existing extent.
- **No `ENOSPC` surprises.** `journal_size` is "pre-allocated and zeroed ... This enables static allocation of disk space so that appends cannot fail with ENOSPC" (`constants.zig:191-195`). Format calls `fallocate` for the fixed zones (`linux.zig:1625-1648`).
- **Cheaper syncs.** "Writes within this file never extend the filesystem inode size reducing the cost of fdatasync()" (`constants.zig:193`).
- **Detects truncation.** On open, if the file is shorter than expected: `@panic("data file inode size was truncated or corrupted")` (`linux.zig:1663-1665`).
- **Raw block devices work.** A block device is just a big file with no inode (Concept 14).
- **Physical determinism.** Every replica has the same zones at the same offsets. Up-to-date replicas have byte-identical grids (`data_file.md`), enabling block-level repair (Concepts 20 and 25).

### 3.4 Alignment and sync rules per zone

- Every zone size and start is a multiple of `sector_size`, asserted at comptime (`vsr.zig:158-173`).
- The grid start is a multiple of `block_size`. `grid_padding` exists only for this: "Aligning blocks this way makes it more likely that they are aligned to the underlying physical sector size" (`vsr.zig:134-137`). `storage.zig:367` asserts padding is never written.
- Only the grid may be written without a synchronous write (`Zone.dsync`, `vsr.zig:203-218`). The comptime block calls this "critical to safety". On Linux it does not matter, since the whole file is opened `O_DSYNC`. On macOS, grid writes are flushed at the end of each compaction bar (Concept 14).

### 3.5 The superblock: why 4 copies

The superblock holds the replica's root: the checkpoint state (pointers into the grid), the VSR view numbers, and recent view headers. It is the one thing that "contains local state for the replica and therefore cannot be replicated remotely. Loss of the superblock would represent loss of the replica and so it must be protected" (`constants.zig:550-551`).

Unlike grid blocks, nothing outside the superblock can hold its checksum. It must defend itself. `data_file.md`:

> To implement atomic update of the superblock, the superblock is physically stored as 4 distinct copies on disk. After startup, replica picks the latest superblock which has at least 2 copies written. Picking just the latest copy would be wrong --- unlike the grid blocks, the superblock stores its own checksum, and is vulnerable to misdirected reads (i.e., a misdirected read can hide the sole latest copy).

**Writing.** A checkpoint or view change builds a new `staging` header (sequence + 1, `parent` = current checksum, Concept 15). It then writes the copies **one at a time**: copy 0, then on its callback copy 1, and so on (`superblock.zig:1183-1206`). After the last copy, it reads all 4 back with the `verify` threshold. Sequential writes mean a crash can tear at most one copy.

**Read thresholds** (`superblock_quorums.zig:53-77`):

| copies | `verify` (after write) | `open` (at startup) |
|---|---|---|
| 4 (default) | 3 | 2 |
| 6 | 4 | 3 |
| 8 | 5 | 4 |

The rule in the code: "the write quorum plus the read quorum must be exactly copies + 1", so they always overlap. And "the open quorum must allow for at least two copy faults, because we update copies in place, temporarily impairing one copy." With 4 copies: one copy torn mid-write plus one copy lost to a sector error still leaves 2.

`superblock_copies` must be 4, 6 or 8: even numbers, "for more efficient flexible quorums" (`constants.zig:548`, compile error at `superblock.zig:594-599`).

**Why not just take the highest sequence?** `superblock.zig:686-697` gives two scenarios:

1. Sequence 7 is written to 3 of 4 copies, the last write is lost. At startup, reads are misdirected to the copy holding sequence 6. Taking "the latest you saw" picks 6 and loses state.
2. Sequence 7 is written to 1 copy, then a crash. At startup that read fails, so the replica recovers at 6 and later writes a *different* sequence 7 to 3 copies. Now 4 copies have sequence 7, one with a different checksum.

A quorum by checksum handles both. Two quorums with the same sequence but different checksums is `error.Fork` (`superblock_quorums.zig:165-170`).

**The `copy` field.** Each copy stores its index, but the field is excluded from the checksum "to ensure that all copies have the same checksum" (`superblock.zig:57-62`, `399-416`). If copy slot 2 contains a header saying `copy = 0`, the read was misdirected. The quorum counts it idempotently, so one physical copy read four times can't fake a quorum (`superblock_quorums.zig:199-258`). If the `copy` byte itself is corrupt (out of range), it is assumed to be in the right slot.

**Repair at open.** After choosing a quorum, the replica rewrites any stale or broken copies of the working version (`superblock.zig:637-638`, "Repair any broken copies").

### 3.6 The WAL: two rings

The WAL is two circular buffers (`journal.zig:16-27`):

- `wal_prepares`: 1024 slots of 1 MiB. Each holds a full prepare (header + body). Prepares are written only up to the sector-rounded `size`, not the full 1 MiB.
- `wal_headers`: 1024 slots of 256 B. Each holds a **redundant copy** of the prepare's header.

A prepare's slot is `op % journal_slot_count` (`journal.zig:63`). Op 1024 overwrites op 0's slot. An entry may only be overwritten after it has been checkpointed (`vsr.md` glossary: "Entries are only overwritten after they have been checkpointed."). Concept 17 explains how the checkpoint interval is sized so this always holds.

**Write order** (`journal.zig:1789-1908`): first the prepare to its prepare slot, then (`write_prepare_header`) the 4 KiB sector of the header ring containing that slot's header. Headers are written a sector at a time, 16 per sector.

**Why a redundant header ring?** `constants.zig:186-188`: "These header copies enable us to disentangle corruption from crashes and recover accordingly." At recovery, the replica reads both rings and compares each slot. A shortened version of the decision table in `journal.zig:1204-1232`:

| Header ring | Prepare ring | Likely cause | Decision |
|---|---|---|---|
| valid, matches prepare | valid | normal | `eql` (@P): nothing to do |
| valid | corrupt | corruption, or a torn append | `vsr` (@B, @C): repair from another replica, *unless* `torn_prepares` proves it was the append in flight at the crash, then `cut_torn`: truncate |
| corrupt | valid, and its op is the highest seen | header sector torn after the prepare landed | `fix` (@F): rewrite header from the local prepare |
| corrupt | valid, not the highest op | can't tell if the prepare is current or from an older wrap | `vsr` (@E) |

The `cut_torn` refinement lives in `recover_slots` (`journal.zig:1278-1287`) and `torn_prepares` (`journal.zig:1345`). It only truncates when every other fault can be ruled out.

The distinction between "torn during a crash" and "corrupted after being acknowledged" is the heart of protocol-aware recovery. Truncating a corrupted committed prepare would lose acknowledged data. Concept 24 covers it in depth.

Why can't one torn write destroy all the redundant headers? `config.zig:175-179` requires at least two sectors of header ring (`journal_slot_count_min = 2 * (4096 / 256) = 32`). And `journal.zig:117-118`: "It's important that the replica doesn't write all redundant headers simultaneously. Otherwise, a crash could lead to a series of torn writes making the entire journal faulty."

### 3.7 Client replies

`client_replies` has one 1 MiB slot per client session (64 in production). From `client_replies.zig:1-20`:

- It stores only the **latest** reply per session, so a client that missed its reply (a dropped packet) can get it again on retry (Concept 27).
- Reply *headers* live in the `client_sessions` trailer in the grid. The zone only holds replies whose body is non-empty.
- Writes are asynchronous and may be coalesced. "ClientReplies guarantees that the latest replies are durable at checkpoint."
- Corrupt replies are repaired from other replicas. If one is corrupt on *all* replicas, the cluster stays available, but that client may be "locked out" until restarted with a new session.

### 3.8 The grid

The grid is "an elastic array of 512KiB blocks" (`data_file.md`) that holds the LSM forest, manifest log, free set and client sessions. It grows as blocks are allocated: the superblock records the logical `storage_size`, computed as `data_file_size_min + highest_acquired_address × block_size` at each checkpoint (`replica.zig:5174-5189`). The `--limit-storage` flag caps it (default 16 TiB, maximum 64 TiB, `config.zig:98-99`). Concept 17 covers the grid in depth.

### 3.9 Docs vs code

> ⚠️ **Docs vs code.** `data_file.md` is explicitly "simplified a bit". Where it diverges, the code wins:
>
> - It says superblock updates "need to write a fair amount of data (several megabytes)". In the current code each copy write is one 8 KiB `SuperBlockHeader` (`superblock.zig:1158`). The large structures (free set, client sessions) moved into grid "trailers". `superblock_quorums.zig:88` even says "now that there are no superblock trailers".
> - Its `SuperBlock` sketch has `manifest_oldest`, `manifest_newest` and `free_set`. The real `CheckpointState` (`superblock.zig:320-397`) has separate `free_set_blocks_acquired` and `free_set_blocks_released` references, a `client_sessions` reference, an unused `snapshots_block` reference, and more.
> - It calls the block reference fields `index` and `checksum`. The code calls it `address`, and addresses start at 1 (`vsr.zig:243-248`).
> - It lists the "main" zones as WAL, superblock and grid. It omits `client_replies` and `grid_padding`, and the on-disk order is superblock first.
>
> A stale comment too: `constants.zig:553-556` talks about worst-case "checkpointing latencies ... of at most 264ms" due to "EWAH compression of our block free set" in the superblock. Since the free set lives in the grid now, this appears outdated. I could not confirm the current number.

## 4. What it means for ashi

### Rust features that help

- **Const layout math.** `Zone` can be a `#[repr(u8)] enum` with `const fn start(self) -> u64` and `const fn size(self) -> Option<u64>`. Use `const _: () = assert!(...)` for every alignment invariant TigerBeetle asserts at comptime.
- **Exact struct sizes.** `#[repr(C)] struct SuperBlockHeader { ... }` with `const _: () = assert!(size_of::<SuperBlockHeader>() == 8192);` and `offset_of!(SuperBlockHeader, view_headers_all) == 4096` (`core::mem::offset_of!` is stable).
- **No padding.** Derive `zerocopy::IntoBytes`, which fails to compile if the struct has implicit padding. Padding bytes would break checksums (Concept 15).

### Patterns

- Compute the whole layout from a single `Config` struct at compile time, as TigerBeetle does. Changing a cluster constant should change one place. Hash the config into a fingerprint stored in the superblock (TigerBeetle's `ConfigCluster.checksum`, `config.zig:196-208`) so a binary built with different constants refuses to open the file.
- Write superblock copies sequentially. Read all copies, group by checksum, apply quorum thresholds. Port `superblock_quorums.zig` almost line for line; it is small and well-tested by `superblock_quorums_fuzz.zig`.
- Keep the two-ring WAL. It is cheap: 256 KiB of headers for 1 GiB of prepares.
- Address the grid from 1 and use `NonZeroU64` for addresses.

### Crates

Little here needs a crate. `zerocopy` (or `bytemuck`) for layout-safe casting is the main candidate. Everything else is arithmetic. TigerBeetle uses none.

### Pitfalls specific to Rust

- `#[repr(Rust)]` (the default) may reorder fields. Every on-disk struct must be `#[repr(C)]` with explicit, zeroed reserved fields.
- `u128` alignment changed: before Rust 1.77 on x86-64, `u128` was 8-byte aligned, now it is 16-byte aligned to match C. If ashi's on-disk structs contain `u128`, assert alignment and offsets explicitly so a toolchain change can't silently move fields.
- `usize` arithmetic for offsets is a trap on 32-bit targets. Use `u64` for all file offsets.
- Don't model zones as separate `File` handles or `BufWriter`s. One file descriptor, `pread`/`pwrite` at computed offsets.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt** the single preallocated file and zone layout. It is simple, deterministic and file-system-agnostic. Consider **adapting** sizes (e.g. `message_size_max`, `clients_max`) to ashi's workload, since they drive most of the fixed overhead.

## 5. Drill questions

### Q1. Recall

List the six zones in on-disk order, with their production sizes. Which one is variable-sized and why is it last?

> _Your answer:_

### Q2. Mechanism

Explain why the superblock needs a quorum of copies at startup when grid blocks need only one copy plus an external checksum. Use the "sequence 7 written to 3 of 4 copies, reads misdirected" scenario.

> _Your answer:_

### Q3. Back-of-envelope

ashi decides on `message_size_max = 2 MiB`, `clients_max = 256`, `journal_slot_count = 1024`, `block_size = 1 MiB`, 4 superblock copies with 24 KiB each.

- What is the size of each fixed zone?
- How big is the grid padding, and where does the grid start?
- How much disk does a freshly formatted replica use before storing any account?

> _Your answer:_

### Q4. What breaks if…

…TigerBeetle wrote all four superblock copies in parallel instead of one at a time, and power failed during the writes?

> _Your answer:_

### Q5. ashi design (open-ended)

The client-replies zone reserves `clients_max × message_size_max` up front, even though most replies are small. Propose an alternative for ashi that uses less space. What would you have to give up (think: determinism, static allocation, repair)?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** `superblock` (96 KiB: 4 copies × 24 KiB), `wal_headers` (256 KiB), `wal_prepares` (1 GiB), `client_replies` (64 MiB), `grid_padding` (160 KiB), `grid` (variable, up to `--limit-storage`, max 64 TiB). The grid is last because it is the only zone that grows; putting it at the end means every other zone's offset is a compile-time constant and growing the grid just extends the file.

**Q2.** A grid block is found through a parent that holds its checksum, so a misdirected or stale read is detected (`unexpected_checksum`) and repaired from a peer. The superblock is the root: nothing holds its checksum, and it cannot be fetched from peers because it holds replica-local state. A copy with a valid internal checksum may still be the wrong (older) version. In the scenario, copies 0 to 2 hold sequence 7 and copy 3 holds 6. If reads are misdirected so all four reads return copy 3, a "highest sequence seen" rule picks 6 and silently rolls back. With quorums, the replica sees a single physical copy (the `copy` field says 3 every time, counted once), finds no quorum of 2, and refuses to open instead of rolling back.

**Q3.**
- superblock: 4 × 24 KiB = 96 KiB.
- wal_headers: 1024 × 256 B = 256 KiB.
- wal_prepares: 1024 × 2 MiB = 2 GiB.
- client_replies: 256 × 2 MiB = 512 MiB.
- Sum before padding: 2 GiB + 512 MiB + 352 KiB. 2 GiB + 512 MiB is a multiple of 1 MiB, so the remainder is 352 KiB. Padding = 1024 − 352 = 672 KiB. Grid start = 2 GiB + 512 MiB + 1 MiB = 2,685,403,136 bytes.
- Fresh replica: about 2.5 GiB before any data. (Also check: the superblock copy padding depends on `clients_max − pipeline_prepare_queue_max`. With `clients_max = 256` it would grow beyond 16 KiB, so the 24 KiB assumption is optimistic. Good catch if you noticed.)

**Q4.** A power cut could tear all four copies at once. The old version would be partly overwritten everywhere and the new version incomplete everywhere. No quorum of either sequence would exist, and the replica could not open: the replica is lost. Sequential writes guarantee at most one copy is mid-write at any moment, so the old version still has at least 3 intact copies (until the new one has at least 2). The `open` threshold of 2 is chosen exactly to tolerate that one in-flight copy plus one more fault.

**Q5.** Options: a smaller fixed slot (e.g. 64 KiB) with large replies spilled to grid blocks; or storing replies only in the grid, referenced from the client sessions trailer. Costs: replies in the grid need block allocation per reply (more free-set churn), must follow copy-on-write and checkpoint rules (Concept 17), and must be byte-identical across replicas to stay repairable. A variable layout also complicates static allocation and the "every write is an overwrite" property. TigerBeetle's choice trades disk (64 MiB is small next to a TiB grid) for simplicity.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
