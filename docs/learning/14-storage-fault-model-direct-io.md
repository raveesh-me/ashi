# Concept 14: Storage fault model, fsyncgate and Direct IO

**Module:** M3 Storage · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/concepts/safety.md`, sections "Storage Fault Tolerance" and "Durability"
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Overview" (storage paragraph), "Direct IO", "Protocol Aware Recovery" and "References"
- `../tigerbeetle/src/constants.zig` (`sector_size`, `direct_io`, `superblock_copies`)
- `../tigerbeetle/src/config.zig` (`direct_io`, `journal_slot_count_min`)
- `../tigerbeetle/src/stdx/io/common.zig` (`sector_size`, `DirectIO`)
- `../tigerbeetle/src/stdx/io/linux.zig` (`open_data_file`, `write`, `fs_supports_direct_io`)
- `../tigerbeetle/src/stdx/io/darwin.zig` (`open_data_file`, `fs_sync`, `write`)
- `../tigerbeetle/src/storage.zig` (latent sector error handling, `start_write`, `flush_sectors`)
- `../tigerbeetle/src/vsr.zig` (`Zone.dsync`, `Zone.verify_iop`)
- `../tigerbeetle/src/testing/storage.zig` (simulated faults, `ClusterFaultAtlas`)
- `../tigerbeetle/src/vopr.zig` (fault probabilities the VOPR picks)
- Rebello et al., "Can Applications Recover from fsync Failures?" (USENIX ATC 2020), linked from `ARCHITECTURE.md`
- Jepsen, "TigerBeetle 0.16.11", section "disk faults", linked from `ARCHITECTURE.md:90`

## 1. The problem

A ledger is only as good as its durability. If TigerBeetle says "transfer committed", that money movement must survive power loss, kernel bugs and dying disks.

Most databases assume the **crash fault model**: the machine may stop at any moment, but whatever the disk said it wrote is there, intact, when you read it back. TigerBeetle's docs say this model "can lead to real data loss for users in the wild" (`safety.md`, "Durability").

Real disks fail in quieter ways. The four you must know:

- **Latent sector error (LSE).** A sector becomes unreadable. "Latent" means you only find out when you read it, possibly months after the write. The read returns `EIO` (input/output error).
- **Silent corruption (bit rot).** The read succeeds but returns wrong bytes. No error code at all.
- **Misdirected IO.** The disk writes correct data to the *wrong* place, or reads from the wrong place. The bytes are self-consistent; they are just not the bytes you asked for.
- **Torn write.** Power fails mid-write. Some sectors of the write are new, some are old. A 1 MiB write is many sectors, and the disk only promises atomicity per sector at best.

How common? `safety.md` cites field studies:

| Fault | SSD (per disk per year) | HDD (per disk per year) |
|---|---|---|
| Silent corruption | 0.031% | 1.4% (enterprise) |
| Misdirected IO | 0.023% | 0.466% (nearline) |

Rare per disk. But a cluster of 6 replicas running for 10 years is 60 disk-years. Over a fleet, "rare" becomes "certain".

Then there is the software layer. The OS sits between you and the disk, and its error handling is weaker than most people think. That is **fsyncgate**.

## 2. The obvious solution, and why it fails

The textbook recipe for durability on Linux:

```
write(fd, buf)    // copy into the kernel page cache; returns immediately
fsync(fd)         // force dirty pages to disk; returns 0 or an error
if error: retry fsync
```

The **page cache** is the kernel's in-memory cache of file contents. `write` only copies into it and marks the page "dirty". The kernel writes it back to disk later. `fsync` asks it to do so now.

### Fsyncgate

In 2018 PostgreSQL developers found that this recipe loses data. Rebello et al. studied it systematically (ATC 2020). Their findings, from the paper:

- ext4, XFS and Btrfs **all mark the pages clean after fsync fails**, "rendering techniques such as application-level retry ineffective".
- ext4 and XFS keep the *new* (unwritten) data in memory. Btrfs reverts to the previous on-disk state.
- So PostgreSQL's retry hit a page cache with no dirty pages. The second fsync "succeeded" without writing anything. PostgreSQL then truncated its WAL, "thereby losing data". The paper notes PostgreSQL "had been using fsync incorrectly for 20 years".
- The fix many projects adopted: crash on fsync failure and recover from the WAL on restart. But the paper finds recovery often *reads through the page cache*, which "does not reflect the persistent state of the system". LevelDB, SQLite and PostgreSQL "seemingly work correctly as long as the relevant data remains in the file-system cache". After eviction or reboot, they return stale data.

Worked example of the trap:

```
t0  write(account_page, balance=500)  → page cache: dirty, 500
t1  fsync → EIO                        → page cache: CLEAN, 500   disk: 100 (old)
t2  app crashes, restarts quickly, reads account_page
      → page cache hit: 500  (looks fine, app "recovers" happily)
t3  memory pressure evicts the clean page
t4  read account_page → disk: 100      (the 400 vanished, silently)
```

Even "panic and restart" is not safe if restart reads through the kernel cache. `ARCHITECTURE.md` "References" summarises the paper exactly that way: "the kernel page cache is a dangerous way to recover the journal, even when restarting from an fsync() failure panic."

### Why checksums alone don't fix it

You might add a checksum to each record. That catches bit rot. It does not catch:

- a misdirected write (the wrong location holds a *valid* record with a valid checksum),
- a lost write (the old, valid record is still there),
- a write that was "durable" only in the page cache.

Concept 15 covers the checksum side. This lesson is about removing the page cache from the picture and modelling the faults explicitly.

## 3. What TigerBeetle does

TigerBeetle's stance, in one line from `safety.md`: "**TigerBeetle assumes that its disk _will_ fail**". And from `ARCHITECTURE.md` "Overview": if `fsync` returns `0` "it could still be the case that when reading this data later it will be found to be corrupted."

The response has four parts:

1. Remove the page cache (Direct IO). Make every write durable when it completes.
2. Treat read errors as corruption, not as fatal errors.
3. Crash on write or fsync errors. Never retry them.
4. Repair corruption from other replicas (Concepts 24 and 25), and test all of this in a simulator that injects every fault class (Concept 29).

### 3.1 Direct IO and the sector size

**Direct IO** (`O_DIRECT` on Linux) tells the kernel to move bytes between your buffer and the device with no page cache copy. TigerBeetle then manages its own cache (the grid cache, Concept 17).

`ARCHITECTURE.md` "Direct IO" gives two reasons: correctness after fsync failure, and "avoiding dependencies and reducing assumptions" (no page cache and no file system required).

Direct IO has a hard rule: buffer address, file offset and length must be aligned, or the syscall fails with `EINVAL`. TigerBeetle fixes the alignment at 4 KiB everywhere:

```zig
// src/stdx/io/common.zig:13-17
/// The minimum size of an aligned kernel page and an Advanced Format disk sector:
/// This is necessary for direct I/O without the kernel having to fix unaligned pages with a copy.
/// ... This should therefore never be less than 4 KiB to be future-proof ...
pub const sector_size = 4096;
```

`src/constants.zig:493-496` re-exports it and asserts `sector_size == 4096`. "Advanced Format" disks have 4 KiB physical sectors. Older disks used 512 bytes, and 4 KiB alignment also satisfies them.

Every IO is checked before it is issued (`src/vsr.zig:224-234`):

```zig
pub fn verify_iop(zone: Zone, buffer: []const u8, offset_in_zone: u64) void {
    ...
    assert(@intFromPtr(buffer.ptr) % constants.sector_size == 0); // address
    assert(buffer.len % constants.sector_size == 0);              // length
    ...
    assert(offset_in_storage % constants.sector_size == 0);       // offset
    if (zone == .grid) assert(offset_in_storage % constants.block_size == 0);
}
```

Buffers are aligned in the type itself: `pub const BlockPtr = *align(constants.sector_size) [constants.block_size]u8;` (`src/vsr/grid.zig:23`). The compiler, not a runtime check, guarantees the address alignment.

Direct IO is on in production. `src/config.zig:221` sets `.direct_io = true` in `default_production`. The test config sets it to `false` (`src/config.zig:239`). The warning in `src/constants.zig:505-507`:

> WARNING: Disabling direct I/O is unsafe; the page cache cannot be trusted after an fsync error, even after an application panic, since the kernel will mark dirty pages as clean, even when they were never written to disk.

### 3.2 Opening the data file on Linux

`open_data_file` (`src/stdx/io/linux.zig:1440`) builds the open flags:

```zig
// src/stdx/io/linux.zig:1453-1457
var flags: posix.O = .{
    .CLOEXEC = true,
    .ACCMODE = if (purpose == .inspect) .RDONLY else .RDWR,
    .DSYNC = true,
};
```

`O_DSYNC` means each `write` returns only after the data (and the metadata needed to read it back) is on stable storage. Combined with `O_DIRECT`, a completed write is a durable write. No separate `fsync` is needed in the data plane.

The mode is passed in by the caller (`src/tigerbeetle/main.zig:161-167`):

- `direct_io_required` in production. If the file system cannot do `O_DIRECT`, TigerBeetle panics with "file system does not support Direct I/O" (`linux.zig:1536-1546`).
- `direct_io_optional` with `--development`. It warns and continues.
- tmpfs is a special case. It cannot do Direct IO but is useful for benchmarks, so TigerBeetle warns "tmpfs is not durable" and skips the check (`linux.zig:1517-1529`).

Support is probed, not assumed: `fs_supports_direct_io` (`linux.zig:1784`) tries to open a scratch file with `O_DIRECT`.

On open, TigerBeetle calls `fsync` on the file and on its parent directory (`linux.zig:1650-1659`). The comment credits Alex Miller (FoundationDB): "always fsync before reading because this prevents us from making decisions on data that was never durably written by a previously crashed process."

Finally, on Linux the per-write `dsync` option is ignored because the whole file is already `O_DSYNC`:

```zig
// src/stdx/io/linux.zig:1284-1286
// Linux dsync is efficient - it's used on every write.
assert(dsync_all);
_ = options;
```

### 3.3 Raw block devices

Because the data file is one fixed-size file (Concept 16), TigerBeetle does not need a file system at all. `open_data_file` accepts a block device such as `/dev/nvme0n1p2`:

- `O_DIRECT` and `O_EXCL` are set (`linux.zig:1495-1512`). On a block device, `O_EXCL` ensures no file system has it mounted and none can mount it while TigerBeetle runs.
- The size comes from the `BLKGETSIZE64` ioctl, and TigerBeetle panics if the device is too small (`linux.zig:1667-1693`).
- On `format`, it refuses unless the first superblock-zone bytes are all zero. This catches pointing it at a disk that holds a real file system or partition table (`linux.zig:1695-1722`).
- It then issues `BLKDISCARD` over the whole device, and only warns if unsupported (`linux.zig:1728-1757`).

No file system means one less layer of software with its own bugs and its own fsync semantics.

### 3.4 macOS: F_NOCACHE and F_FULLFSYNC

macOS has no `O_DIRECT`. `src/stdx/io/darwin.zig` adapts:

- **Page cache bypass:** `fcntl(fd, F_NOCACHE, 1)` (`darwin.zig:1076-1079`).
- **`O_DSYNC` is off on purpose.** The comment at `darwin.zig:1030-1045` says "DSYNC is broken on Darwin".
- **`fsync` is not enough.** Darwin's `fsync` flushes to the drive but not past the drive's own volatile cache. TigerBeetle uses `F_FULLFSYNC` instead (`darwin.zig:1120-1128`):

```zig
/// Darwin's fsync() syscall does not flush past the disk cache. We must use F_FULLFSYNC
/// instead.
fn fs_sync(fd: fd_t) !void {
    // TODO: This is of dubious safety - it's _not_ safe to fall back on posix.fsync unless it's
    // known at startup that the disk (eg, an external disk on a Mac) doesn't support
    // F_FULLFSYNC.
    _ = posix.fcntl(fd, posix.F.FULLFSYNC, 1) catch return posix.fsync(fd);
}
```

- Non-grid writes call `fs_sync` right after `pwrite` (`darwin.zig:848-851`). Grid writes are not synced one by one; the forest calls `flush_sectors` at the end of each compaction bar (`src/lsm/forest.zig:736-737`).

That split comes from `Zone.dsync()` (`src/vsr.zig:203-218`): every zone except `.grid` requires a synchronous write, and a comptime block asserts only the grid may skip it. On Linux the distinction is moot because everything is `O_DSYNC`.

Note the TODO: the fallback to plain `fsync` is admitted to be unsafe. macOS is a development platform for TigerBeetle, not a production one.

### 3.5 What happens on each error

`src/storage.zig` is the real storage layer. Its error policy is asymmetric.

**Read `EIO` → treat as corruption, keep going.** A latent sector error should not crash the replica. TigerBeetle narrows down the bad sectors with a binary search (`storage.zig:255-311`):

```
read 512 KiB → EIO
  split: read first 256 KiB → ok, read second 256 KiB → EIO
    split: 128 KiB → EIO ... down to one 4 KiB sector → EIO
      zero that sector, continue with the next one
```

```zig
// src/storage.zig:293-299
// Zero this logical sector which can't be read:
// We will treat these EIO errors the same as a checksum failure.
assert(target.len > 0);
@memset(target, 0);
```

The zeroed sector then fails its checksum one layer up, and the normal repair path fetches the data from another replica (Concept 25). After a successful single-sector read, the read size grows again (`storage.zig:346-353`, described as "AIMD": additive increase, multiplicative decrease).

**Write `EIO` → panic.** `storage.zig:415`: `error.InputOutput => @panic("latent sector error: no spare sectors to reallocate")`. Disks remap a bad sector to a spare on write. A write error means the spares are gone, and the disk is dying.

**fsync failure → panic, never retry.** `storage.zig:497`: `_ = result catch @panic("fsync failure");`. With Direct IO there is no page cache whose state could mislead recovery. After restart, the replica reads only from the disk, finds whatever is (or isn't) there by checksum, and repairs from peers. This is the fsyncgate lesson applied.

**Write returns 0 bytes → panic** (`storage.zig:436-445`). The comment notes "block devices are known to exhibit this behavior in the wild."

**ENOSPC → fatal.** Space exhaustion is meant to be handled logically via `--limit-storage`, not by the disk filling (`storage.zig:418-426`). On format, the file is preallocated with `fallocate` so later writes cannot hit `ENOSPC` (`linux.zig:1625-1648`).

### 3.6 Torn writes

TigerBeetle never assumes a multi-sector write is atomic. Its defences live in the data layout (Concept 16):

- Every unit (prepare, block, superblock copy) carries a checksum, so a torn unit fails validation.
- The WAL keeps a second, redundant ring of headers. The minimum WAL size exists because of torn writes:

```zig
// src/config.zig:175-179
/// The WAL requires at least two sectors of redundant headers — otherwise we could lose them
/// all to a single torn write. ...
pub const journal_slot_count_min = 2 * @divExact(sector_size, @sizeOf(vsr.Header));
```

- WAL recovery can tell a prepare torn by a crash (safe to truncate) from a corrupt committed prepare (must be repaired). See `torn_prepares` in `src/vsr/journal.zig:1345` and Concept 24.
- The superblock is written as 4 copies, one at a time, so a crash can tear at most one copy (Concept 16).

### 3.7 The fault model, as code: simulated storage

A fault model is only real if you test it. TigerBeetle's model is encoded in the simulator's in-memory disk, `src/testing/storage.zig`. Its fault options (`storage.zig:68-79`):

| Option | Simulates |
|---|---|
| `read_fault_probability` | a read corrupts a sector (LSE / bit rot seen on read) |
| `write_fault_probability` | a write corrupts a sector |
| `write_misdirect_probability` | a whole write lands at another offset in the same zone |
| `crash_fault_probability` | on crash, a pending write corrupts one of its target sectors (torn write) |
| `read_latency_*`, `write_latency_*` | slow disks (gray failure) |

The VOPR picks these per run (`src/vopr.zig:475-478`): read, write and misdirect faults each 0 to 10%, crash faults 80 to 100%.

How each fault is injected:

- **Corruption** flips one bit at a position derived deterministically from the sector's original bytes, so "read-retries don't resolve the corruption" (`storage.zig:486-495`).
- **Never-written sectors** read back as random bytes, not zeros (`storage.zig:497-499`). Code cannot rely on fresh disk being zeroed.
- **Misdirected writes** are modelled with two "overlays" (`storage.zig:160-187`): the intended target keeps its *old* data (a lost write) and a different offset gets the *new* data. At most one misdirect per storage at a time, because "misdirected faults are an order of magnitude less frequent than bit rot, which in turn is an order of magnitude less frequent than LSEs."
- **Torn writes on crash:** `reset()` walks pending writes and corrupts a random target sector (`storage.zig:271-291`).

The crucial constraint is the `ClusterFaultAtlas` (`storage.zig:1015-1110`). Faults are only allowed where the *cluster* can still recover:

```
ClusterFaultAtlas, 3 replicas, quorum_replication = 2  →  faults_max = 1

chunk:      0   1   2   3   4   5   ...
replica 0:  X   .   .   X   .   .
replica 1:  .   X   .   .   .   X
replica 2:  .   .   X   .   X   .
            each chunk is faulty on at most faults_max replicas,
            so at least one healthy copy always exists
```

`faults_max = quorums.replication - 1` (`storage.zig:1077`). A chunk is a WAL header sector, a client reply slot, or a grid block (`zone_chunks`, `storage.zig:1118-1152`). It also never corrupts *every* chunk of one replica. With one replica there is no one to repair from, so WAL, reply and grid faults are disabled (`storage.zig:1042-1049`). With two replicas, grid faults are off too (`vopr.zig:485-487`).

This is the honest statement of the fault model: **TigerBeetle tolerates any storage fault, on any replica, as long as one good copy of each piece of data survives somewhere in the cluster.** If a record is corrupt on all replicas, `safety.md` says "the system would safely halt".

> ⚠️ **Docs vs code.** The header comment of `src/testing/storage.zig:9-11` says the superblock zone permits "one read/write fault ... per area". The code does not do this. `zone_chunks` returns `null` for `.superblock` (`storage.zig:1123-1131`), so no read, write or misdirect faults are ever injected there. The superblock only gets crash (torn-write) faults from `reset()`. The `faulty_superblock` option is accepted but only checked with `maybe()` (`storage.zig:1044`). The code comment's own TODO explains why: a read fault plus a torn write could leave no quorum of 4 copies. The code wins: in the VOPR, superblock faults are crash-only.

There is also a fuzzer for the *real* LSE path: `src/storage_fuzz.zig` runs the production `storage.zig` against a test IO that fails clusters of sectors with `EIO`, exercising the binary-search-and-zero logic.

### 3.8 Jepsen's findings

`ARCHITECTURE.md:86-90` claims durability "even in the presence of latent sector errors and helical corruption" and links Jepsen's TigerBeetle 0.16.11 analysis. From that report (fetched from jepsen.io, not stored in the clone):

- Jepsen built a file-corruption nemesis. It flipped random bits, replaced file chunks with other chunks (misdirected writes), and restored old snapshots of chunks (lost writes). It targeted single zones, such as only the WAL headers or one of the four superblock copies, as well as whole files.
- A **helical** fault corrupts every node, but different chunks on each node. Example from the report: in three nodes, one node corrupts chunks 1, 4, 7, 10 and another corrupts 2, 5, 8. This only works because replicas are "bit-for-bit identical" (Concept 20), so the healthy chunks on other nodes can repair it.
- Their summary: TigerBeetle "offered exceptional resilience to disk corruption, including damage to every replica's files. However, it lacked a way to handle the total loss of a node's data."

The following came from a summarized portion of the report, not text I verified line by line, so confirm before relying on it: two crash bugs on bitflips (in chunk padding and in the superblock `copy` field) fixed in 0.16.26. Helical faults in the WAL could disable the cluster. Rolling back all four superblock copies is outside the fault model. A `tigerbeetle recover` command for lost nodes came later.

The superblock `copy` field case is visible in today's code: `superblock_quorums.zig:239-247` explicitly handles a `copy` number that is out of range because it is not covered by the checksum.

## 4. What it means for ashi

### Rust features that help

- **Alignment in the type.** `#[repr(C, align(4096))] struct Sector([u8; 4096]);` gives a buffer type that is always Direct IO safe. A block can be `#[repr(C, align(4096))] struct Block([u8; 512 * 1024]);`. This is the Rust version of `*align(sector_size) [block_size]u8`.
- **`const` asserts.** `const _: () = assert!(SECTOR_SIZE == 4096);` and `const _: () = assert!(BLOCK_SIZE % SECTOR_SIZE == 0);` mirror the Zig `comptime` blocks.
- **Opening with flags:** `std::os::unix::fs::OpenOptionsExt::custom_flags(libc::O_DIRECT | libc::O_DSYNC)` on Linux. Then `std::os::unix::fs::FileExt::{read_at, write_at}` for `pread`/`pwrite`.

### Patterns

- One `fn verify_iop(buf: &[u8], offset: u64)` that asserts address, length and offset alignment, called on every IO.
- Allocate all IO buffers once at startup with `std::alloc::alloc(Layout::from_size_align(len, 4096)?)` or a static pool of aligned blocks (Concept 11).
- Classify errors explicitly: read `EIO` → zero the sector and report "corrupt"; write `EIO`, fsync error, zero-byte write → `panic!`.
- Probe for Direct IO support at startup; refuse to run in production without it.
- Block-device support: `libc::ioctl(fd, BLKGETSIZE64, &mut size)`. The `nix` crate's `ioctl_read!` macro or `rustix` can wrap this.

### Crates worth evaluating

TigerBeetle has zero dependencies (Concept 13), so every crate is a deliberate choice.

| Crate | Use | Trade-off |
|---|---|---|
| `libc` | raw `O_DIRECT`, `fcntl`, `ioctl`, `fallocate` | minimal, unsafe, de facto required |
| `rustix` | safer syscall wrappers, incl. `fallocate`, `fcntl` | another layer; check it exposes what you need |
| `io-uring` | raw io_uring submission queue (Concept 12) | low-level, no runtime; closest to TigerBeetle |
| `glommio` | io_uring runtime built around Direct IO files | brings its own executor and threading model |
| `tokio` | general async | `tokio::fs` uses a blocking thread pool and the page cache; wrong fit |

### Pitfalls specific to Rust

- **`Vec<u8>` is 1-byte aligned.** Passing it to an `O_DIRECT` read gives `EINVAL`, sometimes only on some kernels or file systems. Use an aligned type.
- **`write_all_at` hides partial writes.** It loops internally. That is fine, but you lose the chance to assert on a zero-byte write, which TigerBeetle panics on.
- **`?` invites retry.** Idiomatic Rust bubbles `io::Error` up, where some caller may retry. For fsync and write errors that is exactly the fsyncgate bug. Make those paths `panic!`.
- **`Drop` swallows errors.** `File`'s `Drop` ignores `close` errors. Never rely on drop for durability; durability must be established before you acknowledge.
- **`mmap` (e.g. `memmap2`) brings the page cache back,** plus `SIGBUS` on IO errors instead of `EIO`. Avoid it for the data file.
- **macOS:** no `O_DIRECT`. Use `libc::fcntl(fd, libc::F_NOCACHE, 1)` and call `libc::fcntl(fd, libc::F_FULLFSYNC)` explicitly. I believe Rust's `File::sync_all` uses `F_FULLFSYNC` on Apple targets, but I have not verified it against a toolchain here. Calling `fcntl` yourself removes the doubt. Treat macOS as dev-only, as TigerBeetle does.
- **Deterministic tests need a storage trait.** Make storage a generic parameter (`Storage: StorageApi`), not `dyn`, so the simulator's fault-injecting storage (Concept 29) compiles to the same code paths.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt.** The fault model and the "panic on write/fsync error, repair on read error" policy are language-independent. Rust's aligned types make Direct IO safer than in C.

## 5. Drill questions

### Q1. Recall

Name the four classes of disk fault from this lesson. For each, say whether the disk returns an error code, and which simulator option in `src/testing/storage.zig` models it.

> _Your answer:_

### Q2. Mechanism

Explain why "panic on fsync failure, then recover from the WAL on restart" is still unsafe for an application that uses buffered IO. Then explain why it *is* safe for TigerBeetle.

> _Your answer:_

### Q3. Back-of-envelope

A ledger cluster runs 6 replicas on enterprise SSDs for 5 years. Using the SSD rates in `safety.md`:

- Roughly how many silent-corruption events should you expect across the cluster?
- How many misdirected-IO events?
- Now assume the same cluster on nearline HDDs for misdirects. How does the answer change?

> _Your answer:_

### Q4. What breaks if…

…TigerBeetle retried a failed read sector-by-sector, but instead of zeroing an unreadable 4 KiB sector it simply returned the whole read as an error to the caller? Trace what happens to a grid block read during compaction.

> _Your answer:_

### Q5. ashi design (open-ended)

ashi wants to be deployable on managed cloud volumes (e.g. AWS EBS) where you cannot use raw block devices and may not control the file system. What is the minimum set of guarantees ashi must probe for or require at startup? Would you ever allow a non-Direct-IO mode in production?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- *Latent sector error:* the read fails with `EIO`. Modelled by `read_fault_probability` (the simulator corrupts the sector instead of returning `EIO`; the real `storage.zig` converts `EIO` into a zeroed sector, so both end up as "checksum fails").
- *Silent corruption:* no error; wrong bytes. `read_fault_probability` and `write_fault_probability` (one bit flipped deterministically).
- *Misdirected IO:* no error; correct bytes at the wrong place. `write_misdirect_probability` (two overlays: stale target, wrong location overwritten).
- *Torn write:* no error at write time (the process died); partial data on read. `crash_fault_probability`, applied in `reset()` to pending writes.

**Q2.** With buffered IO, the failed fsync marks the dirty pages *clean* (ext4, XFS, Btrfs all do this, per Rebello et al.). If the process restarts before those pages are evicted, recovery reads the *new* data from the page cache and believes it is durable. Later the page is evicted and the disk's old contents reappear. Retrying fsync also fails, because there are no dirty pages left to flush. TigerBeetle avoids both traps: with `O_DIRECT` there is no page cache, and with `O_DSYNC` a write is durable when it completes. After a panic, every read goes to the device, and everything read is validated by checksum. Anything missing or corrupt is repaired from another replica.

**Q3.** 6 disks × 5 years = 30 disk-years.
- Silent corruption: 30 × 0.031% ≈ 0.0093, about 1%. Unlikely in one cluster, but across 100 clusters you expect about one.
- Misdirects on SSD: 30 × 0.023% ≈ 0.007, about 0.7%.
- Misdirects on nearline HDD: 30 × 0.466% ≈ 0.14, about 14% per cluster. Across a fleet that is routine.
The point: per-cluster odds look small, but a database vendor's fleet will see these regularly, and each one is a potential silent ledger error if undetected.

**Q4.** One bad sector would fail the entire block read. Compaction needs that block. With an error, the grid would need a separate error path, and a 512 KiB block would be lost because of one 4 KiB sector. TigerBeetle instead zeroes just that sector, so the block fails `read_block_validate` (checksum mismatch). The grid then fetches the block from another replica by `(address, checksum)` (Concept 25), and the caller never sees an error. `ARCHITECTURE.md` "Determinism" puts it as: the read function "doesn't have an error condition." Returning errors would push storage faults into every caller, which is exactly what the design avoids.

**Q5.** Reasonable answer: require (1) `O_DIRECT` support, probed by opening a scratch file; (2) honoured `O_DSYNC` or a working `fdatasync` with device flush; (3) 4 KiB-aligned logical blocks; (4) successful preallocation (`fallocate`) so `ENOSPC` cannot appear later; (5) an `fsync` of the file and directory at open. On EBS, write durability is the provider's promise, but corruption and misdirects can still happen in the stack, so checksums and replica repair still matter. A non-Direct-IO production mode should be refused, or at most allowed with a loud flag, because the fsyncgate failure is silent. TigerBeetle only relaxes this for tmpfs benchmarks and `--development`.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
