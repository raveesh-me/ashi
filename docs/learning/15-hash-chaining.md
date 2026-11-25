# Concept 15: Hash-chaining and checksums stored outside the data

**Module:** M3 Storage · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Hash-Chaining", "Grid" and "Determinism"
- `../tigerbeetle/docs/internals/data_file.md` (block references, superblock copies)
- `../tigerbeetle/docs/concepts/safety.md`, section "Storage Fault Tolerance"
- `../tigerbeetle/src/vsr/checksum.zig` (the algorithm), `src/vsr/checksum_benchmark.zig`
- `../tigerbeetle/src/stdx/vendored/aegis.zig` (vendored AEGIS implementation)
- `../tigerbeetle/src/vsr/message_header.zig` (`Header`, `Request`, `Prepare`, `Reply`, `Block`)
- `../tigerbeetle/src/vsr.zig` (`BlockReference`)
- `../tigerbeetle/src/vsr/grid.zig` (`read_block_validate`)
- `../tigerbeetle/src/lsm/schema.zig` (index block layout, manifest and trailer back-links)
- `../tigerbeetle/src/vsr/superblock.zig`, `src/vsr/superblock_quorums.zig` (superblock `parent`, quorum checks)
- `../tigerbeetle/src/vsr/replica.zig` (prepare `parent`, request `parent` check)
- `../tigerbeetle/src/vsr/client.zig` (client side of the request chain)

## 1. The problem

Concept 14 established that disks lie. They flip bits, lose writes, and put correct data in the wrong place. For a ledger, any of these could silently change a balance.

So every piece of data needs a **checksum**: a short fingerprint computed from the bytes. On read, recompute it and compare. If they differ, the data is corrupt.

The hard questions are:

- **Where do you store the checksum?** Next to the data, or somewhere else?
- **How do you know a piece of data is the *right* data,** not just *valid* data?
- **How do you know a sequence of entries is complete and in order,** with nothing missing, reordered or substituted?

## 2. The obvious solution, and why it fails

The obvious design: put a CRC32 in each record's header, covering the record. Postgres pages and many file formats do this.

```
disk offset 0         offset 4096        offset 8192
┌──────────────────┐┌──────────────────┐┌──────────────────┐
│ crc | block A    ││ crc | block B    ││ crc | block C    │
└──────────────────┘└──────────────────┘└──────────────────┘
```

This catches bit rot. It fails three ways.

**1. Misdirected write.** You write the new version of block B (call it B′). The disk puts it at offset 8192 instead of 4096.

```
offset 4096: crc | B   (old, internally valid: a lost write)
offset 8192: crc | B′  (valid, but it should be C)
```

Read offset 4096: checksum passes, you get stale B. Read offset 8192: checksum passes, you get B′ when you asked for C. Two corruptions, zero detections. An internal checksum only proves the bytes are *self-consistent*. It cannot prove they are *the bytes you wanted*.

**2. CRC32 is weak.** It is 32 bits. The chance a random corruption passes is about 1 in 4 billion. Across petabytes of reads over years, that is not zero. CRCs are designed for burst errors on a wire, not as identities for data.

**3. No ordering.** Each record is checked alone. Nothing tells you that record 41 really comes after record 40, or that nothing was dropped between them.

## 3. What TigerBeetle does

`ARCHITECTURE.md` "Hash-Chaining" defines the idea in two steps:

- compute a checksum for each data unit,
- include this checksum with some parent data, which is also checksummed.

"This is the same structure as in git commits, but applied more generally."

### 3.1 The checksum: AEGIS-128L as a 128-bit MAC with a zero key

`src/vsr/checksum.zig:1-30` explains the choice:

```zig
//! TigerBeetle uses this checksum to:
//! - detect bitrot in data on disk,
//! - validate network messages before casting raw bytes to an `extern struct` type,
//! - hash-chain prepares and client requests to have strong consistency and ordering guarantees.
//!
//! As this checksum is stored on disk, it is set in stone and impossible to change.
//!
//! We need this checksum to be fast (it's in all our hotpaths) and strong (it's our ultimate line
//! of defense against storage failures and some classes of software bugs).
//!
//! Our checksum of choice is based on Aegis:
```

The construction, step by step (from the same comment):

1. **AES** is a block cipher with dedicated CPU instructions (`vaesenc` on x86, AES on ARM). Hardware acceleration gives the speed.
2. **AEGIS** is an AEAD (Authenticated Encryption with Associated Data) scheme built from AES rounds. Given key, nonce, message and "associated data", it produces ciphertext plus an authentication **tag**.
3. Use an empty message and a zero nonce, and AEGIS becomes a **MAC** (message authentication code) over the associated data.
4. Set the key to zero too, and the MAC becomes a plain **checksum**.

In code (`checksum.zig:41-73`):

```zig
const Aegis128LMac_128 = stdx.aegis.Aegis128LMac_128;

fn seed_init() void {
    const key: [16]u8 = @splat(0);
    seed_state = Aegis128LMac_128.init(&key);
}

pub fn checksum(source: []const u8) u128 {
    ...
    var stream = ChecksumStream.init();
    stream.add(source);
    return stream.checksum();
}
```

Key facts:

- **Algorithm:** AEGIS-128L, MAC mode, 128-bit tag.
- **Width:** `u128`. Birthday-bound collisions need about 2⁶⁴ checksums, far beyond any data file.
- **Hardware required:** `assert(std.crypto.core.aes.has_hardware_support)` at comptime (`checksum.zig:47-55`). TigerBeetle will not even compile for a CPU without AES instructions.
- **Frozen forever:** the implementation is vendored from Zig 0.13.0 "to maintain hash stability" (`src/stdx/vendored/aegis.zig:1-2`). A "checksum stability" test hashes 896 fixed cases and compares against a constant (`checksum.zig:167-217`). If a future compiler or library changed one bit of output, every data file would become unreadable.
- **Room for 256 bits:** every header reserves a `checksum_padding: u128` next to each checksum, with a TODO to merge into a `u256` once Zig's `extern struct` supports it (`message_header.zig:23-24`). Index blocks already store child checksums as `u256` slots (`src/lsm/schema.zig:33`, `checksum_size = @sizeOf(u256)`).

**Why not CRC32C or xxHash?** Both are fast. Neither is strong: CRC32C is 32 bits, and xxHash is a non-cryptographic hash. TigerBeetle uses the checksum as an *identity* (see 3.3, blocks are fetched from other replicas by checksum), so collisions must be practically impossible. With AES hardware, AEGIS gets strength at near-hash speed.

> Note on "strong": with a public all-zero key, this is not a secret MAC, and TigerBeetle's docs do not claim protection against a malicious attacker forging collisions. `safety.md` "Security" says TigerBeetle "must be running in a trusted environment". The goal is accident detection with a negligible miss rate, including software bugs.

### 3.2 Two checksums per header, and what "the checksum" covers

Every message, prepare and grid block starts with the same 256-byte `Header` (`message_header.zig:17-76`, `assert(@sizeOf(Header) == 256)`):

```
offset  field
0       checksum           u128   ← covers bytes 16..256 of the header
16      checksum_padding   u128
32      checksum_body      u128   ← covers the body after the header
48      checksum_body_padding
64      nonce_reserved     u128
80      cluster            u128   ← "binds intention" (see 3.7)
96      size, epoch, view, release, protocol, command, replica, ...
128     reserved_command   [128]u8  ← command-specific fields (parent, op, address, ...)
```

```zig
// message_header.zig:108-114
pub fn calculate_checksum(self: *const Header) u128 {
    const checksum_size = @sizeOf(@TypeOf(self.checksum));
    assert(checksum_size == 16);
    const checksum_value = vsr.checksum(std.mem.asBytes(self)[checksum_size..]);
    ...
}
```

`set_checksum` "must be called only after set_checksum_body()" (`message_header.zig:125-128`). So the header checksum covers `checksum_body`, which covers the body. **One 128-bit value transitively covers the whole message.** That value is what gets stored elsewhere as the "pointer".

Why split header and body checksums? The header can be validated after reading just 256 bytes, before trusting `size` and reading the body. That matters on the network, where you `recv` the header first, and for WAL recovery, which reads the header ring without the 1 MiB prepares.

### 3.3 External checksums: block pointers are (address, checksum)

The internal checksum proves self-consistency. To prove *identity*, TigerBeetle stores the expected checksum **outside** the block, in whatever points to it:

```zig
// src/vsr.zig:237-248
/// Reference to a single block in the grid.
///
/// Blocks are always referred to by a pair of an address and a checksum to protect from misdirected
/// reads and writes: checksum inside the block itself doesn't help if the disk accidentally reads a
/// wrong block.
///
/// Block addresses start from one, such that zeroed-out memory can not be confused with a valid
/// address.
pub const BlockReference = struct {
    checksum: u128,
    address: u64,
};
```

When the grid reads a block, the caller supplies both. Validation, in order (`src/vsr/grid.zig:1421-1446`):

```zig
if (!header.valid_checksum()) return .invalid_checksum;           // bit rot in header
if (header.command != .block) return .unexpected_command;         // e.g. read hit a WAL prepare
if (!header.valid_checksum_body(block_body)) return .invalid_checksum_body; // bit rot in body
if (header.checksum != expect.checksum) return .unexpected_checksum;  // valid, but WRONG block
if (!stdx.zeroed(block[header.size..vsr.sector_ceil(header.size)])) return .invalid_padding;
assert(header.address == expect.address);
```

The fourth check is the one an internal checksum can never do. Replay the misdirected write from Section 2:

```
parent (index block) says:  address=7, checksum=0xB…   (wants B)
disk at address 7 holds:    old B, valid, checksum=0xA…
                            → header valid, body valid, checksum 0xA ≠ 0xB → unexpected_checksum

parent says:                address=9, checksum=0xC…   (wants C)
disk at address 9 holds:    B′, valid, checksum=0xB′…
                            → unexpected_checksum
```

Both victims of the misdirect are caught. On any failure the grid does not return an error to the caller. It asks other replicas for the block by `(address, checksum)` (Concept 25). `ARCHITECTURE.md` "Hash-Chaining": "using the checksum as an identifier". Because replicas are byte-identical (Concept 20), the same `(address, checksum)` names the same bytes everywhere.

Note also: the block header includes its own `address` field (`message_header.zig:1583`), and the final `assert(header.address == expect.address)` is a cross-check. Address 0 is reserved as "null" (`invalid_header` rejects `address == 0`, `message_header.zig:1606`), so a zeroed sector can't masquerade as a valid pointer.

### 3.4 The tree of pointers

Pointers are nested, so a single root checksum covers the whole database:

```
superblock (4 copies, internal checksum + quorum)
 │  vsr_state.checkpoint: manifest_oldest/newest (address, checksum)
 │                        free_set_blocks_acquired/released last block (address, checksum)
 │                        client_sessions last block (address, checksum)
 ▼
manifest block N ──previous_manifest_block (checksum, address)──▶ manifest block N-1 ──▶ …
 │  TableInfo: index_block (address, checksum), key_min, key_max
 ▼
index block
 │  [value_block_count_max]u256  checksums of value blocks     (struct-of-arrays)
 │  [value_block_count_max]Key   key_min
 │  [value_block_count_max]Key   key_max
 │  [value_block_count_max]u64   addresses of value blocks
 ▼
value block (sorted values)
```

The index block layout is from `src/lsm/schema.zig:7-12`. The manifest back-link lives in the block header's metadata: `previous_manifest_block_checksum` and `previous_manifest_block_address` (`schema.zig:544-546`). Free set and client session trailers chain the same way through `previous_trailer_block_checksum` / `_address` (`schema.zig:444-448`).

**Struct-of-arrays (SoA)** means storing all checksums together, then all keys, then all addresses, rather than an array of `{checksum, key, address}` structs. A binary search over keys then touches only the key arrays, which is cache-friendly (Concept 9).

### 3.5 Prepare hash chain: the `parent` field

The WAL is a sequence of **prepares** (batches of client events, assigned an op number by the primary). Each prepare carries its predecessor's checksum:

```zig
// message_header.zig:705-716
/// A backpointer to the previous prepare checksum for hash chain verification.
/// This provides a strong guarantee for linearizability across our distributed log
/// of prepares.
parent: u128,
parent_padding: u128 = 0,
/// The checksum of the client's request.
request_checksum: u128,
```

The primary sets it when it creates the prepare (`replica.zig:7404-7417`): `.parent = latest_entry.checksum`, `.op = self.op + 1`.

```
op 100           op 101                 op 102
┌───────────┐    ┌──────────────────┐    ┌──────────────────┐
│checksum=X │◀───│parent=X          │◀───│parent=Y          │
│           │    │checksum=Y        │    │checksum=Z        │
└───────────┘    └──────────────────┘    └──────────────────┘
```

Because Y covers `parent=X`, and Z covers `parent=Y`, knowing Z pins down the entire history before it. `ARCHITECTURE.md`: "via a transitive closure, gives a global consistency guarantee".

Where it is used:

- **Commit only a connected chain.** "Do not commit until the hash chain between `self.commit_min` and `self.op` is fully connected" (`replica.zig:7811-7812`). Before executing a prepare, the replica asserts `prepare.header.parent == header.checksum` of the previous op (`replica.zig:5390-5400`).
- **Anchor across WAL wraps.** The previous op's WAL slot may have been overwritten. The superblock stores the checkpoint's prepare header, which anchors the chain: "we can always use the VSRState to anchor the hash chain" (`replica.zig:5392-5395`).
- **Repair from the head backwards.** A backup that knows the latest op's checksum can accept any earlier prepare, from any replica, if it chains. `repair_header_would_connect_hash_chain` (`replica.zig:7963-7995`) walks forward via `parent` links to the head and accepts the header only if it connects. Its comment walks through how replacing ops without this check would break quorum guarantees.
- **Catch divergence.** `panic_if_hash_chain_would_break_in_the_same_view` (`replica.zig:7282-7303`) treats a break within one view as a bug, not a fault.

`ARCHITECTURE.md` notes the one weak spot: the *latest* prepare has nothing after it to vouch for it. "Backups need extra care (and, during a rare view change, an extra message from the primary) to ensure that the latest prepare in their log is correct". That is Concept 24.

### 3.6 Request chain: checksum of the previous reply

Clients chain too. Each request's `parent` is the `context` of the previous reply (`message_header.zig:570-575`):

```zig
/// Clients hash-chain their requests to verify linearizability:
/// - A session's first request (operation=register) sets `parent=0`.
/// - A session's subsequent requests (operation≠register) set `parent` to the checksum of
///   the preceding reply.
parent: u128 = 0,
```

The round trip, from `client.zig` and `replica.zig`:

```
client                                         replica (primary)
  send request r, parent = ctx(r-1)   ───▶     if request == last+1:
  remember  parent = checksum(r)                  require parent == last_reply.context
                                                  (replica.zig:6598-6609)
                                      ◀───     reply: request_checksum = checksum(r)
                                                      context = checksum of reply header
  assert reply.request_checksum == parent      (client.zig:590)
  parent = reply.context                        (client.zig:598)
```

Why `context` rather than the reply's `checksum`? A reply may be resent in a newer view with an updated `view` field, which changes its `checksum`. `context` is "computed with a fixed view and remains stable if reply is retransmitted in a newer view" (`message_header.zig:978-984`). The replica computes it before stamping the final checksum (`replica.zig:5509-5510`).

`ARCHITECTURE.md` is modest about this one: "these checksums are not as crucial for data validation, [but] they provide a strong proof that the proper ordering of requests is observed." Request and op numbers "have low entropy and may otherwise collide in the event of any correctness bugs" (`message_header.zig:981-982`). A 128-bit checksum does not. This ties into Concept 27 (client sessions).

### 3.7 Superblock version chain

The superblock is the root, so nothing external can hold its checksum. It uses three other defences (Concept 16 covers the layout):

- **4 copies** and a read quorum.
- **A sequence number**, monotonically increasing.
- **A `parent` field**: "The checksum of the previous superblock to hash chain across sequence numbers" (`superblock.zig:77-79`).

Each new version is built from the current one (`superblock.zig:1104-1106`):

```zig
superblock.staging.* = superblock.working.*;
superblock.staging.sequence = superblock.staging.sequence + 1;
superblock.staging.parent = superblock.staging.checksum;
```

At startup, if two versions co-exist (a crash mid-update), the newer one must link to the older one (`superblock_quorums.zig:178-192`):

```zig
if (a.header.sequence + 1 == b.header.sequence) {
    if (a.header.checksum != b.header.parent) {
        return error.ParentNotConnected;
    } ...
```

`ARCHITECTURE.md`: ordering is "constrained weakly by a sequence number and strongly by hash-chaining." A sequence number alone could be corrupted or collide; the chain cannot be faked by accident.

There is a second, coarser chain for checkpoints: `checkpoint_id` is the checksum of the superblock's `CheckpointState` (`superblock.zig:444-446`), and each new checkpoint records `parent_checkpoint_id` and `grandparent_checkpoint_id` (`superblock.zig:926-929`). Every prepare carries the `checkpoint_id` it was prepared against (`message_header.zig:717-726`), "to strictly bound the number of commits that it may take to discover a divergent replica."

### 3.8 "Binds intent"

From `ARCHITECTURE.md`: "Hash chaining **binds intent**: if you know the checksum, then, on receiving any data, you get strong guarantees that this is exactly the data you were looking for. You get protection both from hardware errors and corruption, as well as programming errors that lead to value confusion."

"Value confusion" is the software-bug case. Examples the checksum catches:

- Reading the right address in the wrong zone (a WAL prepare where a block was expected: `unexpected_command`).
- Using a stale pointer after a block was freed and reused (different checksum).
- Mixing up two clusters: the `cluster` field "binds intention into the header" so a client can't accidentally talk to staging instead of production (`message_header.zig:35-38`).

The same idea appears in small places: reserved WAL slots store the slot index as their `op` "to help WAL recovery detect misdirected reads/writes" (`journal.zig:20-21`), and each superblock copy records its `copy` index (`superblock.zig:57-62`).

### 3.9 The git analogy

| git | TigerBeetle |
|---|---|
| blob (file contents), named by hash | value block, referenced by (address, checksum) |
| tree, lists child hashes | index block, lists child checksums and addresses |
| commit, has `parent` hash | prepare, has `parent` checksum |
| commit points at root tree | superblock points at manifest heads |
| `refs/heads/main` (a mutable pointer) | superblock (mutable root, 4 copies) |
| `git fsck` | grid scrubber (Concept 17) |
| fetch missing object by hash from a remote | grid repair: fetch block by (address, checksum) from a replica |

One key difference: git is **content-addressed**: the hash *is* the location in the object store. TigerBeetle's grid is **positional**: blocks live at fixed offsets, so a pointer needs both the address (where to read) and the checksum (what to expect). That is why `BlockReference` is a pair.

## 4. What it means for ashi

### Rust features that help

- `u128` is a native type, and `#[repr(C, align(16))]` headers can mirror TigerBeetle's 256-byte layout exactly.
- Zero-copy casting with `zerocopy` (`FromBytes`, `IntoBytes`, `KnownLayout`) or `bytemuck` (`Pod`). TigerBeetle validates the checksum *before* casting raw bytes to a struct. Keep that order in Rust: checksum the `&[u8]`, then cast.
- `zerocopy`'s `IntoBytes` derive rejects structs with implicit padding, matching TigerBeetle's `assert(stdx.no_padding(Header))`. Padding bytes are uninitialised and would make checksums nondeterministic.
- A newtype `struct Checksum(u128)` and `struct BlockRef { checksum: Checksum, address: NonZeroU64 }`. `NonZeroU64` encodes "address 0 is null" in the type, and `Option<BlockRef>` costs no extra space.

### Candidate checksum crates

Speeds below are rough order-of-magnitude figures from public benchmarks, not measured for this lesson. Benchmark on your target CPU before deciding (TigerBeetle has `src/vsr/checksum_benchmark.zig` as a model).

| Crate | Output | Strength | Rough single-core speed | Notes |
|---|---|---|---|---|
| `aegis` | 128/256-bit tag | cryptographic MAC | tens of GB/s with AES-NI | same family as TigerBeetle; check it has a MAC API and matches TB's test vectors |
| `blake3` | 256-bit | cryptographic hash | a few GB/s (more with SIMD / multithreading) | collision-resistant even with a public key; slower per byte than AEGIS |
| `xxhash-rust` (XXH3-128) | 128-bit | non-cryptographic | tens of GB/s | very fast; fine for random bit rot; no guarantee against structured collisions |
| `crc32c` | 32-bit | error-detecting code | tens of GB/s with SSE4.2 | too narrow to use as an identity |

**Cost per GB, as arithmetic:** at 10 GB/s, 1 GB costs 100 ms of one core, and a 1 MiB prepare costs about 0.1 ms. At 1 GB/s it is 1 s per GB and 1 ms per prepare. Each prepare is checksummed several times (created, received, written, read back during recovery or repair), so the factor matters.

### The case for implementing your own

- The checksum is "set in stone" once data is on disk. A crate update that changes output, or a crate that disappears, is a data-format break. TigerBeetle vendored its implementation for exactly this reason.
- AEGIS-128L is small: an 8-block AES state, an update function and a finalize step. With `core::arch::x86_64::_mm_aesenc_si128` and `core::arch::aarch64::vaeseq_u8` / `vaesmcq_u8` it is a few hundred lines.
- You can then make ashi's checksum bit-for-bit compatible with TigerBeetle's by testing against the vectors in `checksum.zig:116-134` and the stability test. That is useful for differential testing (Concept 31).
- Cost: `unsafe` SIMD intrinsics, and you must handle CPUs without AES (TigerBeetle simply refuses to build).

### Pitfalls specific to Rust

- **Uninitialised padding.** A `#[repr(C)]` struct with padding, cast to bytes, includes garbage. Checksums become random. Use `zerocopy` derives that forbid padding, or make padding explicit fields set to zero.
- **Endianness.** TigerBeetle asserts little-endian (`checksum.zig:214`). Do the same with `#[cfg(not(target_endian = "little"))] compile_error!(...)`.
- **`#[derive(Hash)]` is not a checksum.** `std::hash::Hash` output is not stable across Rust versions or platforms. Never persist it.
- **Lifetimes vs pointers.** It is tempting to model block references as Rust references or `Arc`s. On disk they must be plain `(address, checksum)` values. Keep in-memory caches separate from the persistent pointer type.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt** external (address, checksum) pointers and prepare/superblock chaining as-is. **Adapt** on the checksum: either AEGIS-128L (TB-compatible) or BLAKE3 if you want public-key collision resistance, decided by benchmark.

## 5. Drill questions

### Q1. Recall

What checksum algorithm does TigerBeetle use, how wide is it, and what three steps turn the underlying primitive into a checksum? Why does the code refuse to compile without AES hardware?

> _Your answer:_

### Q2. Mechanism

A disk performs a misdirected write: block B′ (new version of the block at address 7) lands at address 9, which held block C. Explain, check by check through `read_block_validate`, what happens on the next read of address 7 and of address 9. Then explain why an internal-only checksum detects neither.

> _Your answer:_

### Q3. Back-of-envelope

ashi targets 1 million transfers per second, 128 bytes each. Assume each byte of transfer data gets checksummed 5 times end to end (client request, primary prepare, backup receive, WAL validation on recovery, grid block write and later read).

- How many GB/s of checksumming is that?
- What fraction of one core does it cost at 10 GB/s? At 1 GB/s?
- Which candidate algorithm(s) leave headroom on a single-threaded design (Concept 6)?

> _Your answer:_

### Q4. What breaks if…

…replies used `checksum` instead of `context` as the value the client puts in the next request's `parent`, and a view change happened between the primary committing the request and the client receiving the reply?

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle reserves 256 bits for every checksum but uses 128. Should ashi store 128 or 256 bits? Consider disk and memory cost (index blocks, headers), the collision math, compatibility with TigerBeetle, and future migration.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** AEGIS-128L used as a MAC, producing a 128-bit (`u128`) tag. Steps: AEGIS is an AEAD built on AES rounds; with an empty secret message and zero nonce it becomes a MAC over the associated data; with an all-zero key the MAC becomes a checksum. AES hardware (`vaesenc` etc.) is what makes it fast. The software fallback is much slower, and the checksum is on every hot path (network, WAL, grid), so `checksum.zig` asserts `has_hardware_support` at comptime rather than silently running slow.

**Q2.** Read address 7: the disk holds the *old* B (a lost write). Header checksum valid, command is `.block`, body checksum valid, but `header.checksum` is B's, not the B′ the parent expects → `.unexpected_checksum`. Read address 9: holds B′. All internal checks pass, but `header.checksum` (B′) ≠ expected (C) → `.unexpected_checksum`. In both cases the grid repairs from another replica by `(address, checksum)`. An internal checksum only compares the block to itself, and both blocks are internally consistent, so it reports nothing wrong.

**Q3.** 1M × 128 B = 128 MB/s of transfer data. × 5 = 640 MB/s ≈ 0.64 GB/s. At 10 GB/s: 6.4% of a core. At 1 GB/s: 64% of a core, which would destroy a single-core engine's budget. Only the AES-accelerated MAC (AEGIS) or a very fast non-cryptographic hash (XXH3) leave real headroom. BLAKE3 single-threaded sits in between and would need measuring. (Real numbers also include headers and LSM write amplification, so the true multiplier is higher than 5.)

**Q4.** The primary commits and computes the reply in view v. After the view change, a replica resends the reply with `view` updated to the newer view, which changes `checksum`. If the chain used `checksum`, the client would receive a reply whose checksum differs from what the cluster recorded in the client table (built in view v). The client's next request would carry a `parent` that doesn't match `entry.header.context` on the replica, and `on_request` would log "ignoring new request (client bug)" (`replica.zig:6603-6608`). The client would be stuck. `context` is computed once, with the original view, so it is stable across retransmission.

**Q5.** A reasonable answer: 128 bits is enough for accidental collisions (birthday bound ~2⁶⁴), and matches TigerBeetle's actual use. 256 bits doubles pointer size in index blocks (TB already pays this with `u256` slots) and in memory-resident manifest tables. Reserving 256 bits on disk but using 128 (as TB does) keeps the option to upgrade without a format change, at a modest disk cost. If ashi picks BLAKE3 for adversarial resistance, 256 bits is the natural output. If ashi wants byte-compatibility with TigerBeetle for differential testing, it must use TB's exact layout, padding included.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
