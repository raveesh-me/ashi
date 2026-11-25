# Concept 7: Batching

**Module:** M1 Single-node speed · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Batching" (lines 364–376), "Overview" (27–78) and "io_uring" (467–487)
- `../tigerbeetle/docs/concepts/performance.md`, section "Batching, Batching, Batching"
- `../tigerbeetle/docs/coding/requests.md`, section "Batching Events"
- `../tigerbeetle/docs/internals/lsm.md` (bar and beat glossary)
- `../tigerbeetle/docs/TIGER_STYLE.md`, section "Performance"
- `../tigerbeetle/src/config.zig`, `../tigerbeetle/src/constants.zig`
- `../tigerbeetle/src/vsr/multi_batch.zig` (trailer encoding, `event_max`)
- `../tigerbeetle/src/vsr/message_header.zig` (`Header` size)
- `../tigerbeetle/src/state_machine.zig` (`batch_max`, `tree_values_count`)
- `../tigerbeetle/src/lsm/groove.zig`, `../tigerbeetle/src/lsm/table.zig`, `../tigerbeetle/src/lsm/forest.zig`
- `../tigerbeetle/src/inspect_snapshot.zig` (a test snapshot that prints the derived sizes)
- `../tigerbeetle/src/clients/c/tb_client/context.zig`, `../tigerbeetle/src/clients/c/tb_client/batch.zig` (client-side batching)
- Line numbers are from the clone at commit `a2a3c70`.

## 1. The problem

Every unit of work in a replicated database carries **fixed costs** that do not depend on how much data it holds:

- a network round trip to the cluster;
- a consensus round: assigning an op number, writing the write-ahead log (WAL), collecting a quorum of acknowledgements;
- a checksum and hash-chain link (Concept 15);
- syscalls and IO completions;
- prefetching from storage (Concept 10);
- a reply message.

If one transfer pays all of these, the fixed costs dominate. A transfer's actual accounting work is tiny: a few lookups and adds.

The goal: make each fixed cost pay for as many transfers as possible, while keeping latency acceptable when load is light.

## 2. The obvious solution, and why it fails

The obvious design is one request per transfer, as in a typical SQL or key-value API.

Model it. Let `C` be the fixed cost per request and `c` the per-transfer work. With `b` transfers per request:

```
cost per transfer = c + C / b
```

Illustrative numbers (assumed for the sketch, not measured in TigerBeetle): `C = 200 µs` (consensus plus fsync plus syscalls, amortized), `c = 0.5 µs`.

| Batch size `b` | Cost per transfer | Throughput (one core) |
|---:|---:|---:|
| 1 | 200.5 µs | ~5,000 /s |
| 100 | 2.5 µs | ~400,000 /s |
| 8,189 | ~0.52 µs | ~1.9 M /s |

At `b = 1` the database is a consensus engine that occasionally adds two numbers.

The second naive fix is "batch on a timer: wait 5 ms, then send what you have". That fails the other way. Under light load, every transfer waits 5 ms for nothing.

## 3. What TigerBeetle does

TigerBeetle batches at every layer, and each layer batches the one below. The docs list them (`ARCHITECTURE.md:368–376`):

> - Individual transfers are aggregated into a prepare. This amortizes replication and prefetch IO overhead.
> - Changes from 32 prepares are aggregated in memory before being written to disk together
> - Changes from 1024 prepares are aggregated before the checkpoint is atomically advanced by overwriting the superblock
> - The LSM tree operates mostly in terms of tables and value blocks, each aggregating many individual records.

The first, second and fourth bullets match the code. The third does not; see 3.4.

```
transfers ──▶ client batch ──▶ request/prepare ──▶ bar ──────────▶ checkpoint
 (128 B)      (≤ 8,189)        (1 MiB message)     (32 prepares)    (960 prepares)
                                    │                   │
                                    ▼                   ▼
                              WAL slot, quorum,    LSM table: ≤ 262,080 transfers
                              hash-chain, prefetch  in ≤ 65 value blocks of 512 KiB
```

### 3.1 Transfers per request and prepare: 8,190 and 8,189

**Terms:**
- A **request** is the client's message.
- The primary turns it into a **prepare**: the same body plus a header with an op number, a timestamp and a checksum link to the previous prepare (`ARCHITECTURE.md:34–43`).
- One prepare equals one consensus op and one WAL slot.

The inputs, all from the source:

| Constant | Value | Where |
|---|---|---|
| `message_size_max` | `1 * MiB` = 1,048,576 B | `src/config.zig:159` |
| `@sizeOf(Header)` | 256 B | `src/vsr/message_header.zig:72` |
| `message_body_size_max` | `message_size_max - @sizeOf(vsr.Header)` = 1,048,320 B | `src/constants.zig:235` |
| `@sizeOf(Transfer)` | 128 B | `src/tigerbeetle.zig:113` |

Dividing gives `1,048,320 / 128 = 8,190` exactly. That is the "8,190" in `performance.md` and in the comment at `src/constants.zig:647`.

But the current `create_transfers` operation is **multi-batch encoded**. A client may pack several independent batches into one message. Their lengths go in a **trailer** at the end of the body (`src/vsr/multi_batch.zig:1–40`):

- one `u16` per batch, plus a `u16` "postamble" holding the batch count;
- padded up to a multiple of the element size, "to keep system invariants" (`multi_batch.zig:10–11`).

`trailer_total_size` (`multi_batch.zig:101–118`) computes it:

```zig
const trailer_unpadded_size: u32 =
    (@as(u32, options.batch_count) * @sizeOf(TrailerItem)) + @sizeOf(Postamble);
...
return stdx.div_ceil(trailer_unpadded_size, options.element_size) * options.element_size;
```

For one batch of transfers, that is 2 + 2 = 4 bytes, padded to 128 bytes. So one transfer slot is lost:

```
(1,048,320 − 128) / 128 = 8,189
```

`event_max` (`multi_batch.zig:499–551`) implements exactly this. A test pins the number (`multi_batch.zig:655–656`):

```zig
const batch_size_max = 8189; // maximum number of elements in a single-batch request.
assert(batch_size_max == @divExact(buffer_size - element_size, element_size));
```

⚠️ **Docs vs code.** `docs/concepts/performance.md` says "up to 8,190 transfers per query" (line 27), and `ARCHITECTURE.md:35` says "a batch of 8 thousand". The client-facing limit for `create_transfers` is **8,189** (`docs/coding/requests.md` table; client READMEs). 8,190 survives only for the deprecated non-multi-batch operations (`deprecated_create_transfers_unbatched`). The server sizes its LSM tables for the larger of the two via `@max` (`src/state_machine.zig:361–371`). Use 8,189 as the API limit and 8,190 as the internal sizing bound.

⚠️ **Stale comment.** `src/constants.zig:226–232` describes `message_size_max` as 2 MiB ("2 MiB is 16,384 transfers"). The configured value is `1 * MiB` (`config.zig:159`). The code wins: 1 MiB.

What one prepare amortizes over its ~8 K transfers:

- one op number, one header checksum, one hash-chain link;
- one WAL write of up to 1 MiB, which is sector-aligned and sequential-friendly;
- one replication fan-out and one quorum of `prepare_ok`;
- one prefetch phase with up to 32 parallel reads (Concept 10);
- one reply message.

### 3.2 The pipeline: prepares in flight

- `pipeline_prepare_queue_max = 8` (`config.zig:155`, `constants.zig:255`): up to 8 prepares replicate concurrently.
- Requests waiting behind them are bounded by `pipeline_request_queue_max = (clients_max + 1) -| pipeline_prepare_queue_max` (`constants.zig:260`).
- With the production `clients_max = 64` (`config.zig:228`) that is 57. `inspect_snapshot.zig:23–24` prints `prepare_queue 8` and `request_queue 57`.
- Each client has at most one request in flight (`constants.zig:257–259`).

### 3.3 The bar: 32 prepares per LSM flush

`lsm_compaction_ops = 32` (`config.zig:164`). The doc comment (`constants.zig:645–654`):

> "Number of prepares accumulated in the in-memory table before flushing to disk. This is a batch of batches. Each prepare can contain at most 8_190 transfers. With lsm_compaction_ops=32, 32 prepares are processed to fill the in-memory table with 262_080 transfers."

Music terms from `docs/internals/lsm.md:7–8`:
- **beat** = one op (`op % lsm_compaction_ops`), a single step of compaction work;
- **bar** = `lsm_compaction_ops` beats.

Compaction work is spread evenly across the beats of a bar instead of done in one burst. `Forest.compact` computes the beat from the op (`src/lsm/forest.zig:384–391`):

```zig
const compaction_beat = op % constants.lsm_compaction_ops;
const first_beat = compaction_beat == 0;
const half_beat = compaction_beat == @divExact(constants.lsm_compaction_ops, 2);
const last_beat = compaction_beat == constants.lsm_compaction_ops - 1;
```

The bar also sets the **table size**. A tree's maximum table size is "one bar of that tree's per-batch values" (`src/lsm/groove.zig:322–323`):

```zig
const table_value_count_max = constants.lsm_compaction_ops *
    @field(groove_options.batch_value_count_max, field.name);
```

For the transfer object tree that is `32 × 8,190 = 262,080` values. The account object tree is `32 × 2 × 8,190 = 524,160` because each transfer updates two accounts (`state_machine.zig` `tree_values_count`, around line 4730). `inspect_snapshot.zig:57,68` prints both (`T=524160`, `T=262080`).

### 3.4 The checkpoint: 960 prepares, not 1024

A **checkpoint** makes the grid durable and atomically advances the superblock, so older WAL entries can be overwritten (`docs/internals/vsr.md:9`; Concept 17).

The interval is derived, not configured (`constants.zig:40–42`):

```zig
pub const vsr_checkpoint_ops = journal_slot_count -
    lsm_compaction_ops -
    lsm_compaction_ops * stdx.div_ceil(pipeline_prepare_queue_max * 2, lsm_compaction_ops);
```

Plugging in `journal_slot_count = 1024` (`config.zig:158`), `lsm_compaction_ops = 32` and `pipeline_prepare_queue_max = 8`:

```
1024 − 32 − 32 × ceil(16 / 32) = 1024 − 32 − 32 = 960
```

The margins exist so the WAL never overwrites an entry that a checkpoint quorum still needs (`constants.zig:44–74`):
- one bar, because the last bar before a checkpoint lives only in memory;
- two pipelines' worth, rounded up to a bar.

The comptime asserts also force `vsr_checkpoint_ops % lsm_compaction_ops == 0`: **a checkpoint is a whole number of bars** (960 = 30 bars). `inspect_snapshot.zig:33–34` prints `checkpoint_ops 960 / journal_slot_count 1024`.

⚠️ **Docs vs code.** `ARCHITECTURE.md:373–374` says "Changes from 1024 prepares are aggregated before the checkpoint". 1024 is the WAL length (`journal_slot_count`). The checkpoint interval in the code is **960**.

### 3.5 LSM tables and blocks

Storage is a grid of equal blocks. `block_size = 512 * KiB` (`config.zig:161`).

A table is one **index block** plus up to N **value blocks** (`src/lsm/schema.zig:7–16`). The value block capacity is computed at comptime (`src/lsm/table.zig:115–121`):

```zig
const block_value_count_max = @divFloor(block_body_size, value_size);
const value_blocks = div_ceil(value_count_max, block_value_count_max);
```

For transfers:
- `(524,288 − 256) / 128 = 4,094` transfers per value block;
- `ceil(262,080 / 4,094) = 65` value blocks per table.

`inspect_snapshot.zig:68` confirms `B=4094 BC=65`.

A block is the unit of:
- disk IO;
- checksumming (Concept 15);
- caching (the grid cache);
- repair between replicas (Concept 25).

So one 512 KiB read or one checksum covers ~4 K records.

### 3.6 Little's law and the client

**Little's law:** in a stable system, `L = λ × W`.
- `L` = average number of items in the system.
- `λ` = throughput (arrival rate).
- `W` = average time each item spends inside.

Rearranged: `λ = L / W`. **For a fixed latency, throughput is bounded by how much work is in flight.**

Apply it to one TigerBeetle client. It has one request in flight. If the round trip is `W = 1 ms`:

- 1 transfer per request: `λ = 1 / 1 ms = 1,000` transfers/s.
- 8,189 per request: `λ = 8,189 / 1 ms ≈ 8.2 M` transfers/s. The client is no longer the bottleneck.

Batching is how TigerBeetle raises `L` without raising the number of connections, threads or locks.

The pipeline gives the server-side bound. At most `8 × 8,189 = 65,512` transfers are being replicated at once. At 1 M transfers/s, Little's law says an average transfer spends at most ~65 ms in that queue when it is full. That is a latency cap from a static limit.

### 3.7 Adaptive batch size under light load

The docs: "under light load, the batches automatically become smaller, trading unnecessary throughput for better latency" (`performance.md`). The mechanism is in the client, `Context.batch_enqueue` (`src/clients/c/tb_client/context.zig:570–626`):

```zig
// Nothing inflight means the batch should be submitted right now.
if (self.client.request_inflight == null) {
    ...
    return self.batch_send(batch);
}
// otherwise: batch.enqueue(...) into self.pending
```

- **Idle:** the user's batch is sent **immediately**. There is no timer.
- **Busy:** the batch waits in `pending`. `Batch.enqueue` (`src/clients/c/tb_client/batch.zig:204–264`) tries to merge it into an existing pending batch of the **same operation**, if the combined events, trailer and expected reply still fit.
- When the in-flight reply returns, the next pending (merged) batch is sent (`context.zig:811–815`).

The result, again by Little's law: **batch size ≈ arrival rate × round-trip time**.

```
light load:  λ small → ~1 event waiting per RTT → send at once, latency ≈ 1 RTT
heavy load:  λ large → many events arrive during one RTT → they merge, up to 8,189
```

No tuning knob, no timer. The batch grows exactly as fast as load demands. An API gateway (Concept 4) applies the same idea across many end users.

### 3.8 Syscall batching

io_uring "combines batching and concurrency" (`ARCHITECTURE.md:469–470`). `IO.run_for_ns` (`src/stdx/io/linux.zig:100–128`) flushes all queued submissions with one `io_uring_enter`, then drains completions in bulk. Concept 12 covers the details.

## 4. What it means for ashi

### Patterns to adopt

- **Make the batch the API unit.** `fn create_transfers(&mut self, events: &[Transfer], out: &mut [CreateTransferResult])`, never a per-transfer API on the server.
- **Derive every size from a few constants at compile time.** Use `const` items and `const fn`, and verify with `const _: () = assert!(...)`:

```rust
pub const MESSAGE_SIZE_MAX: usize = 1 << 20;
pub const HEADER_SIZE: usize = 256;
pub const BODY_SIZE_MAX: usize = MESSAGE_SIZE_MAX - HEADER_SIZE;
pub const LSM_COMPACTION_OPS: usize = 32;
const _: () = assert!(BODY_SIZE_MAX % core::mem::size_of::<Transfer>() == 0);
```

- **Derive the checkpoint interval** from the WAL length, bar and pipeline the way TigerBeetle does. Assert that it is a multiple of the bar.
- **Client batching like `batch_enqueue`:** send now if idle; merge while a request is in flight. No `sleep`/timer-based flushing.

### Rust specifics

- Zero-copy batch decoding: a `&[u8]` body becomes a `&[Transfer]`. This requires `#[repr(C)]`, known alignment and length checks (Concept 9). `bytemuck::cast_slice` or `zerocopy` can do it; a hand-written `unsafe` cast with asserts is what a zero-dependency design would use.
- Avoid `Vec<Transfer>` growth inside the batch path. Preallocate buffers of `BODY_SIZE_MAX` once (Concept 11).
- Async runtimes encourage one task per request. That fights batching, because each task wants its own round trip. Keep the batch as the unit of scheduling.

### Crates worth evaluating (TigerBeetle has zero dependencies)

| Crate | Use | Trade-off |
|---|---|---|
| `bytemuck` / `zerocopy` | Safe casting of batch bodies | Small and well-audited, but still a dependency; you can write the same checks yourself |
| `io-uring` | Batched syscall submission | Low-level; you design the loop |

### Pitfalls

- A trailer or envelope format that is not a multiple of the element size breaks alignment of the next element. TigerBeetle pads the trailer for exactly this reason.
- Choosing batch limits independently at each layer. In TigerBeetle the table size, stash size and checkpoint interval are all *derived* from the batch limit and the bar. Change one and the others follow.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt**. The constants could be **adapted** if ashi picks a different message size, but the derivation chain (message → batch → bar → table → checkpoint) is worth copying exactly.

## 5. Drill questions

### Q1. Recall

List the batching levels in TigerBeetle from smallest to largest, with the real constant for each: transfers per prepare, prepares per bar, prepares per checkpoint, transfers per value block, value blocks per transfer table.

> _Your answer:_

### Q2. Mechanism

Why is the `create_transfers` limit 8,189 when `1,048,320 / 128 = 8,190`? Walk through the trailer math, and explain why the trailer is padded to the element size instead of being 4 bytes.

> _Your answer:_

### Q3. Back-of-envelope (Little's law)

An API gateway holds one TigerBeetle client. Round trip to the cluster is 2 ms, and end users submit 300,000 transfers/s in total.

- What is the steady-state average batch size?
- What happens to batch size and latency if load drops to 200 transfers/s?
- What if load rises to 10 M transfers/s?

> _Your answer:_

### Q4. What breaks if…

…someone changes `pipeline_prepare_queue_max` from 8 to 24 and leaves everything else alone? Recompute `vsr_checkpoint_ops`. Which comptime assertion protects you, and what is the margin for?

> _Your answer:_

### Q5. ashi design (open-ended)

ashi wants lower tail latency for small payment flows while keeping TigerBeetle-class throughput. Would you keep TigerBeetle's "send immediately if idle, otherwise merge" client rule, or add a server-side batching window? Where does each choice put the queueing delay, and how would you measure the difference?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- ≤ 8,189 transfers per `create_transfers` request/prepare. The internal sizing bound is 8,190.
- Up to 8 prepares in flight (`pipeline_prepare_queue_max`).
- 32 prepares per bar (`lsm_compaction_ops`).
- 960 prepares per checkpoint (`vsr_checkpoint_ops`, 30 bars). It is not 1024: that is the WAL length.
- 4,094 transfers per 512 KiB value block.
- ≤ 65 value blocks per transfer table, which holds ≤ 262,080 transfers (`32 × 8,190`).

**Q2.**
- `create_transfers` is multi-batch encoded. The trailer holds one `u16` count per batch plus a `u16` postamble: 4 bytes for one batch.
- `trailer_total_size` rounds it up to a multiple of the element size (128 B). So one 128-byte slot goes to the trailer: `(1,048,320 − 128) / 128 = 8,189`.
- Padding keeps the body an exact multiple of the element size. Every event then stays at a 128-byte stride and cache-line aligned within the 64-byte-aligned body. Validation can also use `@divExact` instead of handling remainders.

**Q3.**
- `L = λ × W = 300,000/s × 0.002 s = 600` transfers per batch on average.
- At 200/s: `L = 0.4`. Most of the time nothing is in flight, so each transfer is sent alone as soon as it arrives. Latency ≈ one RTT (2 ms). Throughput is not needed.
- At 10 M/s: `L` would be 20,000, but the cap is 8,189 per request. The client sends full batches back to back. Its throughput ceiling is `8,189 / 2 ms ≈ 4.1 M/s`. The queue grows until backpressure (bounded client queues) pushes back on users. You would need more clients or gateways, and the cluster's commit rate becomes the limit.

**Q4.**
- `1024 − 32 − 32 × ceil(48 / 32) = 1024 − 32 − 64 = 928`. It stays a multiple of 32, so the asserts at `constants.zig:69–73` still pass: `928 + 32 + 48 ≤ 1024`, `928 ≥ 24`, `928 % 32 == 0`.
- Other asserts matter too. `pipeline_prepare_queue_max <= clients_max + 1` holds for 64 clients. The JV bitset assert `pipeline_prepare_queue_max + 1 <= 128` holds. `view_change_headers_suffix_max >= pipeline_prepare_queue_max + 1` (`constants.zig:307`) **fails**, because the default is `8 + 1`. So it would not compile without also raising that value. The comptime asserts are what protect you.
- What the margin is for: a prepare from the previous WAL wrap may only be overwritten once a quorum has reached the next checkpoint. Two pipelines' worth of slack, rounded to a bar, guarantees that.

**Q5.** Open-ended. Key points:
- **TigerBeetle's rule:** queueing happens on the client while one request is in flight. Idle latency is one RTT and nothing is added. Under load, latency ≈ RTT plus waiting for the in-flight request.
- **A server-side window:** adds the window to *every* request at low load, but can merge across clients into fuller prepares, reducing consensus rounds.
- **Measure** p50/p99/p999 latency versus offered load, from low to saturation, with a closed-loop and an open-loop generator. Watch prepare fill ratio (transfers per prepare) and prepares/s.
- A good answer notices that TigerBeetle already merges across clients at the gateway, and that the server's pipeline already queues requests (`pipeline_request_queue_max`).

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
