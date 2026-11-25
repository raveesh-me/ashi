# Concept 9: Mechanical sympathy

**Module:** M1 Single-node speed · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Mechanical Sympathy" (lines 329–362), "Hash-Chaining" (580–583), "Star Replication" (616–625) and "Direct IO" (505–521)
- `../tigerbeetle/docs/concepts/performance.md`, section "Extreme Engineering"
- `../tigerbeetle/docs/TIGER_STYLE.md`, section "Performance"
- `../tigerbeetle/src/tigerbeetle.zig` (`Account`, `Transfer` and their comptime asserts)
- `../tigerbeetle/src/config.zig`, `../tigerbeetle/src/constants.zig` (`cache_line_size`, `sector_size`, IOPS limits)
- `../tigerbeetle/src/message_pool.zig` (buffer alignment)
- `../tigerbeetle/src/lsm/set_associative_cache.zig` (tags, SIMD search, CLOCK)
- `../tigerbeetle/src/lsm/binary_search.zig` (`@prefetch`, branchless search)
- `../tigerbeetle/src/lsm/schema.zig`, `../tigerbeetle/src/lsm/k_way_merge.zig` (struct-of-arrays)
- `../tigerbeetle/src/lsm/groove.zig` (prefetch workers), `../tigerbeetle/src/vsr/grid.zig`
- `../tigerbeetle/src/vsr/checksum.zig` (AEGIS on AES instructions)
- `../tigerbeetle/src/vsr/replica.zig` (`on_prepare` doc comment)
- `../tigerbeetle/src/stdx/stdx.zig` (`fastrange`, `bytes_as_slice`), `../tigerbeetle/src/stdx/huge_page_allocator.zig`
- Line numbers are from the clone at commit `a2a3c70`.

## 1. The problem

A CPU can run almost any program, but some formulations run 10–100× faster than others on the same hardware. The difference comes from how the program meets the hardware's physical limits.

**Mechanical sympathy** (a term from racing, popularized in software by Martin Thompson of LMAX) means shaping the algorithm to fit those limits (`ARCHITECTURE.md:331–334`).

TigerBeetle names four resources, the "four primary colors of computation": **network, storage, memory, CPU** (`ARCHITECTURE.md:336–341`). TigerStyle adds two characteristics for each, **bandwidth and latency**, and says to sketch all eight before writing code (`TIGER_STYLE.md`, Performance).

Rough latencies for a modern server. These are general hardware figures, not from the clone, but the orders of magnitude are what matter:

```
L1 cache hit           ~1 ns
L2 cache hit           ~4 ns
L3 cache hit           ~10–20 ns
DRAM access            ~80–100 ns      ← a cache miss costs ~100 L1 hits
NVMe read              ~10–100 µs
datacenter round trip  ~100–500 µs
```

For a ledger aiming at 1 µs per transfer (Concept 6), **10 cache misses per transfer use the entire budget**.

## 2. The obvious solution, and why it fails

The obvious approach: write clean, general code, then profile and fix hot spots.

```rust
struct Transfer {                 // typical "clean" model
    id: Uuid,
    debit: Arc<Account>,          // pointer chase → likely cache miss
    credit: Arc<Account>,         // another one
    amount: BigDecimal,           // heap-allocated digits → another
    metadata: HashMap<String, String>,
}
```

- Each transfer touches 4–6 scattered heap objects. That is 4–6 likely cache misses, roughly 0.5 µs of stalls before any work.
- The layout is decided by the allocator, not by you. Nothing is aligned to cache lines or sectors.
- Profiling later shows "everything is a little slow". No single hot spot remains to fix, because the cost is spread across the design.

TigerStyle's answer: "The best time to solve performance, to get the huge 1000x wins, is in the design phase, which is precisely when we can't measure or profile." (`TIGER_STYLE.md`, Performance)

## 3. What TigerBeetle does

### 3.1 Memory: cache lines and the 128-byte objects

A **cache line** is the unit the CPU moves between memory and cache. TigerBeetle fixes it at 64 bytes (`src/config.zig:153`, exported as `constants.cache_line_size` at `src/constants.zig:487`):

```zig
cache_line_size: comptime_int = 64,
```

`Account` and `Transfer` are both exactly **128 bytes = two cache lines**. This is enforced at compile time (`src/tigerbeetle.zig:28–32` and `111–115`):

```zig
comptime {
    assert(stdx.no_padding(Transfer));
    assert(@sizeOf(Transfer) == 128);
    assert(@alignOf(Transfer) == 16);
}
```

- `extern struct` gives C layout: fields in declared order, no reordering.
- `no_padding` (`src/stdx/stdx.zig:504`) proves the compiler inserted no hidden padding. Every byte is accounted for, and the bytes on the wire and on disk are fully defined.

Byte layouts, derived from the field order:

```
Transfer (128 B)                                   Account (128 B)
line 0 ┌───────────────────────────────┐           line 0 ┌───────────────────────────────┐
  0    │ id                   u128     │             0    │ id                   u128     │
 16    │ debit_account_id     u128     │            16    │ debits_pending       u128     │
 32    │ credit_account_id    u128     │            32    │ debits_posted        u128     │
 48    │ amount               u128     │            48    │ credits_pending      u128     │
line 1 ├───────────────────────────────┤           line 1 ├───────────────────────────────┤
 64    │ pending_id           u128     │            64    │ credits_posted       u128     │
 80    │ user_data_128        u128     │            80    │ user_data_128        u128     │
 96    │ user_data_64 u64 │ ud_32 u32 │ timeout u32  96    │ user_data_64 u64 │ ud_32 │ reserved │
112    │ ledger u32 │ code u16 │ flags u16 │        112    │ ledger u32 │ code u16 │ flags u16 │
120    │ timestamp            u64      │           120    │ timestamp            u64      │
       └───────────────────────────────┘                  └───────────────────────────────┘
```

Why this shape works:

- **The fields prefetch needs come first.** Line 0 of a `Transfer` holds the id and both account ids, which is everything Concept 10's prefetch needs to enqueue lookups (`state_machine.zig:1393`, `1447–1448`).
- **Power-of-two size.** Element `i` of a batch is at `i << 7`, with no multiply.
- **Fixed size, no pointers.** Values are copied, compared and checksummed as plain bytes. LSM trees store them by value (Concept 18).
- **u128 everywhere for ids and amounts.** No overflow-prone `u64` balances, and no variable-length decimals.

⚠️ **Docs vs code.** `docs/concepts/performance.md` says "a transfer object is 128 bytes in size, cache-line aligned". The struct's own alignment is **16** (`@alignOf(Transfer) == 16`), not 64. Cache-line placement comes from the **buffers** that hold transfers:

- message buffers are `align(constants.sector_size)` (4,096; `src/message_pool.zig:134`);
- `body_used()` returns `[]align(constants.cache_line_size) u8` (`message_pool.zig:147`);
- the 256-byte header is 4 whole cache lines.

So each transfer in a message body starts on a 64-byte boundary and never straddles a third line. The alignment is a property of the container, not the type.

`ARCHITECTURE.md:355–356` also says "All data for a particular operation can be found in a single cache line", then "the size of a Transfer object is two cache lines". The second statement is the precise one. The balance check `debits_exceed_credits` (`tigerbeetle.zig:34–37`) reads `debits_pending`/`debits_posted` from line 0 and `credits_posted` and `flags` from line 1.

**Zero-copy.** The message body is reinterpreted in place as `[]const Transfer`, with no parsing. `stdx.bytes_as_slice` (`src/stdx/stdx.zig:261`) asserts that the length is an exact multiple and applies `@alignCast`, which is checked in safe builds:

```zig
.exact => {
    assert(bytes.len % @sizeOf(T) == 0);
    return @alignCast(std.mem.bytesAsSlice(T, bytes));
},
```

**TLB pressure.** The server's backing allocator is `stdx.huge_page_allocator` (`src/tigerbeetle/main.zig:68`). On Linux it applies `MADV_HUGEPAGE` "so that the kernel may back them with 2 MiB transparent huge pages, reducing TLB pressure for large allocations" (`src/stdx/huge_page_allocator.zig:10–12`).

### 3.2 Struct-of-arrays (SoA)

**Array-of-structs (AoS)** stores `[{key, checksum, address}, …]`. **Struct-of-arrays (SoA)** stores `{keys: […], checksums: […], addresses: […]}`.

SoA wins when a loop reads only one field. Every cache line fetched is then full of useful data.

TigerBeetle uses SoA in its hottest search structures:

- **LSM index blocks** (`src/lsm/schema.zig:7–12`):

  ```
  │ [value_block_count_max]u256   │ checksums of value blocks
  │ [value_block_count_max]Key    │ the minimum/first key in the respective value block
  │ [value_block_count_max]Key    │ the maximum/last key in the respective value block
  │ [value_block_count_max]u64    │ addresses of value blocks
  ```

  Finding the value block for a key only scans the dense `keys_min`/`keys_max` arrays. The 32-byte checksum and 8-byte address are touched once, for the block that matches. `ARCHITECTURE.md:581–582`: "On disk, an array of child pointers is stored as Struct-of-Arrays (SoA)."

- **Set-associative cache tags** are kept apart from values: "Since tags are 16-32x smaller than keys, they can also be kept hot in cache." (`set_associative_cache.zig:112–117`)
- **K-way merge tournament tree**: `loser_keys` and `loser_ids` are separate arrays, each `align(64)` (`src/lsm/k_way_merge.zig:33–34`).
- **Manifest log** block references: "This is a struct-of-arrays of `BlockReference`s." (`src/lsm/manifest_log.zig:81`)

### 3.3 SIMD and the set-associative cache

The object caches and the grid block cache are **set-associative caches**. Each key hashes to one **set** of `ways` slots; the value can live only in those slots. This is how CPU caches themselves work.

The defaults (`src/lsm/set_associative_cache.zig:15–22`):

```zig
pub const Layout = struct {
    ways: u64 = 16,
    tag_bits: u64 = 8,
    clock_bits: u64 = 2,
    cache_line_size: u64 = 64,
    ...
};
```

- A **tag** is the low 8 bits of the key's hash. Checking 16 one-byte tags is far cheaper than comparing 16 full keys.
- The 16 tags of a set are 16 bytes, exactly one 128-bit SIMD register. `search_tags` compares all of them in one vector operation (`set_associative_cache.zig:290–296`):

```zig
inline fn search_tags(tags: *const [layout.ways]Tag, tag: Tag) Ways {
    const x: @Vector(layout.ways, Tag) = tags.*;
    const y: @Vector(layout.ways, Tag) = @splat(tag);
    const result: @Vector(layout.ways, bool) = x == y;
    return @bitCast(result);
}
```

- The result is a 16-bit mask of candidate ways. Only those get a full key compare.
- The search loop then iterates all ways unconditionally "to help the OOO execution" (out-of-order execution; `set_associative_cache.zig:274`).
- **Eviction uses CLOCK Nth-Chance** with 2-bit counters packed into a bit array (`set_associative_cache.zig:122–140`), not a linked-list LRU. There are no per-entry pointers to chase and no list updates on every hit.
- **Set selection uses `fastrange`** (`stdx.zig:1159–1164`): a 128-bit multiply and a shift instead of a modulo. Integer division is one of the slowest ALU operations.

**Checksums use AES hardware.** TigerBeetle's checksum is AEGIS-128L used as a MAC with a zero key. "Hardware acceleration is what provides speed" via `vaesenc`/`vaesdec` instructions (`src/vsr/checksum.zig:18–20`). Checksums run on every message and block, so they must be nearly free.

### 3.4 CPU prefetching and branch prediction

The word "prefetch" has two meanings in TigerBeetle:

- **Disk-to-memory prefetch:** loading accounts before commit. That is Concept 10.
- **Memory-to-cache prefetch:** a CPU instruction hinting that a cache line will be needed soon.

The second appears in the LSM binary search (`src/lsm/binary_search.zig:59–97`). Each iteration prefetches **both** possible next midpoints, at one quarter and three quarters, before comparing. Whichever way the comparison goes, the next probe's line is already on its way:

```zig
const options: std.builtin.PrefetchOptions = .{ .rw = .read, .locality = 0, .cache = .data };
@prefetch(@as(CacheLineBytes, @ptrCast(@alignCast(one_quarter))) + i, options);
@prefetch(@as(CacheLineBytes, @ptrCast(@alignCast(three_quarters))) + i, options);
```

The comment cites "ARRAY LAYOUTS FOR COMPARISON-BASED SEARCHING" (arXiv 1509.05053). The pointer arithmetic runs with runtime safety off unless `constants.verify` is set: "We need to use pointer arithmetic and disable runtime safety to avoid bounds checks, otherwise prefetching would harm the performance instead of improving it." (`binary_search.zig:70–74`)

**Branch prediction.** A CPU guesses which way each `if` goes and runs ahead. A wrong guess costs a pipeline flush. A binary search's direction is a coin flip, so TigerBeetle marks the branch unpredictable, which hints the compiler to emit a branch-free conditional move (a hint, not a guarantee; `binary_search.zig:109–112`):

```zig
if (take_upper_half) {
    @branchHint(.unpredictable);
    offset = mid;
}
```

At the batch level, branches become predictable through homogeneous batches (Concept 8): every iteration of the commit loop takes the same path for the same operation. ARCHITECTURE puts it this way: CPUs "can both sprint and parkour. But it is so much better at sprinting!" (`ARCHITECTURE.md:358–359`)

### 3.5 Storage: saturate the device, never oversubscribe it

An NVMe SSD serves many requests in parallel; one request at a time leaves most of it idle. TigerBeetle "tries to keep it saturated", and "care is taken to not oversubscribe the storage" (`ARCHITECTURE.md:348–351`).

Every IO class has a static concurrency limit (`src/config.zig:116–119`, `130–131`; summed at `src/constants.zig:510–517`):

| IO class | Read max | Write max |
|---|---:|---:|
| Journal (WAL) | 8 | 32 |
| Client replies | 1 | 2 |
| Grid (LSM blocks) | 32 | 32 |
| Superblock | 1 | 1 |
| **Total (`iops_read_max` / `iops_write_max`)** | **42** | **67** |

Inside those budgets:
- prefetch runs `Grid.read_iops_max` = 32 parallel lookups (`src/lsm/groove.zig:1329`);
- compaction queues up to 16 value-block reads and 16 writes (`constants.zig:667–670`).

The device sees deep, steady queues, and there is no unbounded backlog to blow up tail latency. Static limits like these are Concept 11's main theme.

Storage alignment matters as well:
- `sector_size` is asserted to be 4,096 (`constants.zig:493–496`), so Direct IO (Concept 14) never needs a kernel bounce copy;
- `block_size` is 512 KiB (`config.zig:161`), large enough to amortize per-IO overhead (Concept 7).

⚠️ **Docs wording.** `ARCHITECTURE.md:349–350` says "when compaction needs to read a table from disk it enqueues writes for several of the table's blocks". From context, and from `lsm_compaction_queue_read_max`, this means *reads*. It looks like a typo.

### 3.6 Network: spread bandwidth, tolerate latency

⚠️ **Docs vs code.** `ARCHITECTURE.md:343–346` (Mechanical Sympathy) says the primary "sends each prepare to just two backups, relying on the backups to forward it further". The code, and the later "Star Replication" section (`ARCHITECTURE.md:616–625`), say the opposite. The doc comment on `on_prepare` (`src/vsr/replica.zig:1997–2013`):

> "The primary broadcasts this prepare message to every other replica and standby, in parallel to writing to its own journal. Star replication prioritizes latency: with Flexible Paxos the primary can commit after the fastest prepare_ok quorum returns, without waiting for a ring of forwards."

The code wins: **star replication**. The trade is more primary egress bandwidth for lower and more predictable commit latency. Concept 23 goes deeper.

Other network choices:
- `tcp_nodelay = true` (`config.zig:114`) turns off Nagle's algorithm. TigerBeetle already batches, so kernel-side batching would only add delay.
- `tcp_rcvbuf = 4 * MiB` (`config.zig:109`), with a comment warning that bigger buffers cause latency spikes (`constants.zig:440–442`).
- 1 MiB messages carry ~8 K transfers each, so per-packet and per-message overhead is negligible (Concept 7).

## 4. What it means for ashi

### Rust features that help

- `#[repr(C)]` plus explicit alignment gives TigerBeetle's `extern struct` layout:

```rust
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Transfer {
    pub id: u128, pub debit_account_id: u128, pub credit_account_id: u128, pub amount: u128,
    pub pending_id: u128, pub user_data_128: u128,
    pub user_data_64: u64, pub user_data_32: u32, pub timeout: u32,
    pub ledger: u32, pub code: u16, pub flags: u16, pub timestamp: u64,
}
const _: () = assert!(core::mem::size_of::<Transfer>() == 128);
const _: () = assert!(core::mem::align_of::<Transfer>() == 16);
```

- Aligned buffers: `#[repr(C, align(4096))] struct Sector([u8; 4096]);`, or allocate with `std::alloc::Layout::from_size_align(len, 4096)`.
- Stable intrinsics in `core::arch`:
  - `_mm_prefetch` for cache prefetch;
  - `_mm_cmpeq_epi8` + `_mm_movemask_epi8` for the 16-tag search (SSE2 is baseline on x86-64);
  - `vceqq_u8` on aarch64.
- Recent stable Rust has `std::hint::select_unpredictable` for branch-free selects. Verify availability on your toolchain.

### Patterns

- One `const` assertion block per wire type: size, alignment, and "no padding" (sum of field sizes == size). Rust has no built-in `no_padding`. `zerocopy`'s derives (`IntoBytes`) and `bytemuck`'s `Pod` derive reject padded structs.
- SoA for anything searched by one field: index blocks, cache tags, merge trees.
- A set-associative cache with packed tags and CLOCK counters, instead of `HashMap<K, V>` plus a linked-list LRU.
- Cast message bodies to `&[Transfer]` after checking length and alignment, never field-by-field parsing.

### Crates worth evaluating (TigerBeetle has zero dependencies)

| Crate | Use | Trade-off |
|---|---|---|
| `zerocopy` / `bytemuck` | Checked casts, padding checks | Small and audited; still a dependency |
| `soa_derive` | Generate SoA containers | Proc-macro complexity; hand-written SoA is short |
| `std::simd` (`portable_simd`) | Portable SIMD | Nightly only at time of writing; `core::arch` is stable |
| `aegis` | AEGIS-128L | Check that it uses AES-NI/ARMv8 crypto paths |

### Rust-specific pitfalls

- **`u128` alignment changed.** Older Rust (before 1.77) used 8-byte alignment for `u128` on x86-64, while C/LLVM use 16. Always write `align(16)` explicitly and assert it, so a toolchain change can't silently alter the on-disk layout.
- **Rust reorders fields** in non-`#[repr(C)]` structs. Never cast bytes to a type without `repr(C)`.
- **`Vec<T>` only guarantees `align_of::<T>()`.** A `Vec<u8>` body is byte-aligned, so casting it to `&[Transfer]` is undefined behavior unless you check.
- **Bounds checks in hot loops.** Assert lengths up front, or use `chunks_exact` and iterators, so LLVM can drop per-iteration checks.
- **`Arc` / `Box` inside records** turn a 128-byte value into pointer chasing.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt** the 128-byte, `repr(C)`, zero-copy records and SoA search structures exactly. They are wire and disk formats, and Rust expresses them fully. **Adapt** the SIMD and prefetch code to `core::arch` with per-architecture paths.

## 5. Drill questions

### Q1. Recall

Name the four primary colors of computation and the two characteristics TigerStyle asks you to sketch for each. Then state the three comptime assertions on `Transfer` and what each one guarantees.

> _Your answer:_

### Q2. Mechanism

`Transfer` has `@alignOf == 16`, yet the docs call it "cache-line aligned". Explain exactly how a transfer inside a message body ends up on a 64-byte boundary. What would break if the message header were 200 bytes instead of 256?

> _Your answer:_

### Q3. Back-of-envelope

A cluster of 6 replicas sustains 1,000,000 transfers/s in full 8,189-transfer prepares of ~1 MiB each, using star replication.

- How many prepares per second is that?
- What is the primary's replication egress in MiB/s and roughly in Gbit/s?
- How would the 2-backup forwarding scheme in the old docs change the primary's egress?

> _Your answer:_

### Q4. What breaks if…

…a contributor "simplifies" the set-associative cache into a `std.HashMap` keyed by account id with a doubly-linked-list LRU? Cover memory layout, allocation, branch behavior and SIMD.

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle fixes `cache_line_size = 64` as a cluster-wide constant. Apple M-series and some ARM server cores use 128-byte cache lines (and some Intel cores prefetch adjacent 64-byte line pairs). Should ashi's on-disk and on-wire layouts depend on cache-line size at all? Which structures would you tune per CPU, and which must never change?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- The colors: network, storage, memory, CPU. The characteristics: bandwidth and latency.
- `stdx.no_padding(Transfer)`: no hidden padding bytes, so the layout is fully defined and identical across compilers and replicas. This matters for checksums and determinism.
- `@sizeOf(Transfer) == 128`: exactly two cache lines; power-of-two stride; 8,190 fit in a 1 MiB body.
- `@alignOf(Transfer) == 16`: `u128` fields are naturally aligned, and the type's alignment is pinned so it can't change silently.

**Q2.**
- Message buffers are 4,096-byte aligned (`message_pool.zig:134`). The header is 256 bytes, which is 4 cache lines. `body_used()` returns a slice typed `align(cache_line_size)` (`message_pool.zig:147`).
- Transfer `i` starts at `256 + 128·i`, always a multiple of 64. So it occupies exactly two whole lines.
- With a 200-byte header, the body would start at offset 200. That is not 64-aligned (and not even 16-aligned: 200 mod 16 = 8).
  - Every transfer would straddle three cache lines.
  - The `align(cache_line_size)` slice type would be a lie, and `@alignCast` would panic in safe builds.
  - Casting to `[]const Transfer` would be misaligned for `u128`.
  - Sector-aligned Direct IO of the body would also suffer.
- TigerBeetle asserts `@sizeOf(Header) == 256` (`message_header.zig:72`) partly for this.

**Q3.**
- `1,000,000 / 8,189 ≈ 122` prepares/s.
- Star: the primary sends each prepare to 5 others: `122 × 1 MiB × 5 ≈ 610 MiB/s`, about 5.1 Gbit/s egress. Add ~122 MiB/s of client ingress and the reply traffic.
- With 2-backup forwarding, primary egress drops to `122 × 2 ≈ 244 MiB/s`, about 2 Gbit/s. The load spreads to the backups, but commit latency now includes forward hops for some quorum members, and a slow forwarder delays everyone behind it.
- TigerBeetle chose latency and tail tolerance; a 10–25 GbE link absorbs the extra egress.

**Q4.**
- **Layout:** a generic hash map with a linked list scatters nodes across memory. Each hit chases list pointers and updates `prev`/`next`, which writes to cold lines.
- **Allocation:** list nodes and rehash growth allocate at runtime, which violates static allocation (Concept 11) and causes latency spikes on resize.
- **Branches:** probing and list maintenance add data-dependent branches.
- **SIMD:** you lose the 16-way tag compare in one vector instruction and the dense tag array that stays hot in L1/L2.
- **Bounds:** you lose CLOCK's O(1), branch-light eviction with 2-bit counters, and the hard bound on entries per set.
- **Determinism:** iteration or eviction order may depend on a hash seed unless fixed.

**Q5.** Open-ended. Key points:
- **On-wire and on-disk formats must never depend on the host CPU:** record sizes, header size, block size and sector alignment. They must be identical across replicas and upgrades. TigerBeetle's `cache_line_size` is in `ConfigCluster`, whose checksum must match cluster-wide (`config.zig:196–208`).
- **In-memory-only structures can be tuned per CPU:** cache way counts, tag array grouping, prefetch distance, and padding to avoid false sharing (irrelevant single-threaded, but relevant if any helper threads exist).
- **128-byte records still work on 128-byte-line machines:** one record per line, which is arguably better.
- A good answer proposes a compile-time `CACHE_LINE` used only for memory-only layouts, keeps 128-byte records, and benchmarks on both x86 and ARM.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
