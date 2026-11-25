# Concept 24: Recovery that tells crashes apart from corruption; NACKs

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Protocol Aware Recovery", "Hash-Chaining" and the "References" entry for the SDC 2018 / FAST'18 talk
- `../tigerbeetle/docs/internals/vsr.md`, sections "Glossary" (nack), "Protocol: Normal", "Protocol: View-Change", "Protocol: Repair WAL", "Quorums"
- `../tigerbeetle/docs/concepts/safety.md`, section "Storage Fault Tolerance"
- `../tigerbeetle/src/vsr/journal.zig`: `Ring` (16–45), `headers_per_sector` (47), the torn-write assert (117–126), `headers_redundant` (222–236), `dirty`/`faulty` (268–280), `prepare_checksums` (282–293), `recover_prepare_callback` (1135), the decision-table comment (1170–1239), `recover_slots` (1240), `torn_prepares` (1359), `recover_slot` (1485), `write_prepare` (1789), `write_prepare_header` (1856), `write_prepare_on_write_header` (1910), case comments (2204–2301), `recovery_cases` (2302), `RecoveryDecision` (2353), `recovery_case` (2430), `header_ok` (2484)
- `../tigerbeetle/src/vsr/replica.zig`: `Status` (42–56), head recovery in `open` (770–830), `on_get_prepare` (3044–3121), `send_join_view` nack bits (8813–8915), `JVQuorum` doc comment (11554–11650), `quorum_headers` (11846–11985)
- `../tigerbeetle/src/vsr.zig`: `quorums` (1212–1268)
- `../tigerbeetle/src/config.zig` (104–160), `../tigerbeetle/src/constants.zig` (189–198, 234, 493–496, 520–523)
- Paper: Alagappan et al., "Protocol-Aware Recovery for Consensus-Based Storage", FAST'18 (linked from `ARCHITECTURE.md` and `vsr.md`; not in the clone)

## 1. The problem

A replica restarts. It reads its write-ahead log (WAL: the log of operations, written before they are applied). One entry fails its checksum.

That one bad checksum can mean two very different things:

1. **Crash during append (a "torn write").** The power went out while the entry was being written. The entry was never fully on disk. So this replica never told anyone it had the entry. Throwing it away is safe.
2. **Corruption after the fact.** The entry was written, flushed, and acknowledged to the primary. Later the disk lost it: a bit flip, a latent sector error (a sector that can no longer be read), or a misdirected write (correct data written to the wrong place). This replica was counted toward the quorum that committed the entry. Throwing it away may destroy a committed transaction.

For a ledger, case 2 is a lost transfer. The client got "success". The money moved. Now the record is gone, and a later view change may reuse that op number for a different request.

TigerBeetle's stance (`safety.md`): disks *will* fail, at rates the docs quote from field studies (silent corruption on 0.031% of SSDs per year, misdirected IO on 0.023% of SSDs per year). Concept 14 covers that fault model. This concept is about what the consensus layer does with it.

## 2. The obvious solution, and why it fails

### Obvious fix A: truncate at the first bad checksum

Many logs assume "a bad checksum at the tail means a torn write". On restart they cut the log there. This is the classic crash-consistency model.

A worked example with 3 replicas (replication quorum 2, see Concept 22):

```
op 10 prepared on A (primary) and B. Quorum = 2, so op 10 commits.
Client receives "transfer T created".

B's disk silently corrupts op 10.        B restarts, sees bad checksum,
                                          truncates its log at op 10.
A crashes.
View change with B and C:  B has no op 10 (truncated), C never got it.
New primary assigns op 10 to a different request.
```

Transfer T is gone, although it was committed. One disk fault plus one crash lost data in a cluster that "tolerates one failure". The ARCHITECTURE references call this out directly: the PAR talk explains "why it would be disastrous to truncate the journal when encountering a checksum mismatch."

### Obvious fix B: refuse to start on any corruption

Safe, but availability collapses. With six replicas and 1 GiB of WAL each, the chance that *some* replica has *some* bad sector is not small. Halting turns a recoverable single-sector fault into an outage.

### Obvious fix C: always ask the other replicas for a fresh copy

This works when the entry is committed: by quorum intersection, someone else has it. It fails for an entry that was only *possibly* committed and exists nowhere else. That is exactly the ARCHITECTURE.md counter-example in §3.5 below. Waiting forever for a copy nobody has is a liveness bug.

The root issue: **a checksum mismatch alone can't tell "never written" from "written, then lost".** The fix needs (a) more local evidence and (b) cooperation from the cluster.

## 3. What TigerBeetle does

### 3.1 The paper in one paragraph

Protocol-Aware Recovery (PAR, Alagappan et al., FAST'18) argues that storage faults can't be hidden behind the storage interface; recovery must know the consensus protocol. Its protocol, CTRL (corruption-tolerant replication), (1) keeps extra local information so a replica can tell a crash from a corruption, and (2) lets the cluster safely discard an uncommitted entry if enough replicas positively say they never had it. TigerBeetle cites CTRL by name: the `JVQuorum` doc comment says "Refer to the CTRL protocol from Protocol-Aware Recovery" (`replica.zig:11560`), and `quorum_headers` says "This is an abbreviated version of Protocol-Aware Recovery's CTRL protocol" (`replica.zig:11958`).

> The paper's internals (e.g. exactly how CTRL stores its identifiers) are summarized from memory; they are not in the clone. Everything below about TigerBeetle is verified against the source.

### 3.2 The WAL has two rings: prepares, and redundant headers

The WAL is "two contiguous circular buffers on disk" (`journal.zig:16`):

```
 zone wal_headers  (journal_size_headers = 1024 × 256 B = 256 KiB)
 ┌──────────┬──────────┬─────┬──────────┐
 │ sector 0 │ sector 1 │ ... │ sector 63│   16 headers per 4 KiB sector
 │ h0..h15  │ h16..h31 │     │          │
 └──────────┴──────────┴─────┴──────────┘

 zone wal_prepares (journal_size_prepares = 1024 × 1 MiB = 1 GiB)
 ┌──────────────┬──────────────┬─────┬────────────────┐
 │ slot 0       │ slot 1       │ ... │ slot 1023      │  each slot padded to
 │ hdr + body   │ hdr + body   │     │                │  message_size_max
 └──────────────┴──────────────┴─────┴────────────────┘

 slot = op % journal_slot_count
```

Constants, from the source:
- `journal_slot_count: u32 = 1024` (`config.zig:158`).
- `message_size_max: u32 = 1 * MiB` (`config.zig:159`).
- `@sizeOf(Header) == 256` (`message_header.zig:72`), and `sector_size == 4096` (`constants.zig:493–496`). So `headers_per_sector = 4096 / 256 = 16` (`journal.zig:47`).
- `journal_size_headers = journal_slot_count * @sizeOf(vsr.Header)` and `journal_size_prepares = journal_slot_count * message_size_max` (`constants.zig:197–198`).

Each prepare already contains its own header. The headers ring stores a **second, redundant copy** of each header. The `journal_slot_count` doc comment states the purpose: "These header copies enable us to disentangle corruption from crashes and recover accordingly." (`constants.zig:186–188`).

### 3.3 Write ordering is the trick

A prepare write goes in two steps (`journal.zig:1789–1956`):

```
write_prepare(message)
  └─ write the whole prepare (sector-padded) into its prepares slot
       └─ write_prepare_header: copy header into headers_redundant[slot],
          write the 4 KiB sector of the headers ring that holds it
            └─ write_prepare_on_write_header: clear dirty + faulty bits
```

And a backup sends `prepare_ok` only "once it has written the prepare to the WAL" (`vsr.md`, "Protocol: Normal", step 4).

So the redundant header acts as a **commit record for the local write**:

| On disk after restart | What it means |
|---|---|
| prepare valid, redundant header for the same op | Normal. Both landed. |
| prepare corrupt, redundant header is **from an older wrap** (an op one lap behind) | Most likely we crashed mid-append: the header for this op was never written, so we never acked it. A torn write, but only if `torn_prepares` can prove it (§3.6); otherwise it is treated as corruption. |
| prepare corrupt, redundant header **for this op** | The prepare was fully written (the header came after it), then lost. We may have acked it. Corruption. |
| prepare valid, redundant header from an older wrap | We crashed after the prepare write but before the header write. Fix the header locally. |

Two more details make this robust:
- **Not all header sectors can tear at once.** A comptime assert requires more header sectors than concurrent journal writes: `@divExact(headers_size, constants.sector_size) > constants.journal_iops_write_max` (`journal.zig:124–126`). With defaults that is 64 sectors > 32 writes (`config.zig:117`). A crash can tear at most 32 sectors, so some redundant headers always survive.
- **Misdirected IO is detectable.** `header_ok` rejects a header whose `cluster` is wrong, whose `command` is not `prepare`, or whose `op % slot_count` doesn't match the slot it was read from (`journal.zig:2484–2503`). Reserved (empty) slots store `op = slot index` for the same reason (`journal.zig:19–20`).

### 3.4 Two bits per slot: `dirty` and `faulty`

The journal keeps two bitsets (`journal.zig:268–280`):
- **dirty:** "we have not prepared the entry, or need to repair a faulty entry."
- **faulty:** "we prepared and then lost the entry" through corruption, a misdirected write, or a latent sector error. `faulty` implies `dirty`.

The distinction matters for nacking (§3.6). A dirty-but-not-faulty slot means "I never had it". A faulty slot means "I may have had it, and lost it".

The guiding rule is stated at `journal.zig:1171–1172`: "When in doubt about whether a particular message was received, it must be marked as faulty to avoid nacking a prepare which was received then lost/misdirected/corrupted."

A faulty slot's redundant header is written back with its checksum zeroed (`journal.zig:1574`). So if the replica crashes again before repairing, the slot is still faulty on the next boot. It doesn't silently become "reserved", which would let it nack.

### 3.5 The recovery decision table

On startup, the journal reads every prepare slot (`recover_prepare_callback`, `journal.zig:1135`). It keeps a prepare's header only if the header checksum, the body checksum *and* the zero padding are all valid (`journal.zig:1147–1158`). Then `recover_slots` (`journal.zig:1240`) classifies each slot by matching 11 boolean facts against a table. This is the table from `recovery_cases` (`journal.zig:2302–2343`), reproduced exactly:

```
        Label  Decision      Header  Prepare Compare
               R>1   R=1     ok  nil ok  nil op⌈ op>₁ op>₂ ✓∑  op= op< view
        @A     vsr   vsr     0   _   0   _   _   a0   a0   _   _   _   _
        @B     vsr   vsr     1   1   0   _   _   a0   _    _   _   _   _
        @C     vsr   vsr     1   0   0   _   _   a0   _    _   _   _   _
        @D     vsr   fix     0   _   1   1   _   _    a0   _   _   _   _
        @E     vsr   fix     0   _   1   0   0   0    a0   _   _   _   _
        @F     fix   fix     0   _   1   0   1   0    a0   _   _   _   _
        @G     fix   fix     1   1   1   0   _   0    _    _   _   _   _
        @H     cut   unr     _   _   1   0   _   1    _    _   _   _   _
        @I     cut   unr     1   0   1   0   _   0    1    _   _   a0  _
        @J     cut   unr     1   0   1   1   _   _    1    _   _   a0  _
        @K     vsr   vsr     1   0   1   1   _   _    0    _   _   _   _
        @L     nil   nil     1   1   1   1   _   _    _    a1  a1  a0  a1
        @M     fix   fix     1   0   1   0   _   0    0    0   0   1   _
        @N     vsr   vsr     1   0   1   0   _   0    0    0   0   0   _
        @O     vsr   vsr     1   0   1   0   _   0    0    0   1   a0  a0
        @P     eql   eql     1   0   1   0   _   0    0    1   a1  a0  a1
```

Legend (from the code comment):
- `R>1`: replica_count > 1 (or a standby). `R=1`: a single-replica cluster.
- `ok`: valid checksum, cluster, slot and command. `nil`: operation is `reserved` (empty slot).
- First `ok nil` pair is the **redundant header**; second pair is the **prepare's own header**.
- `op⌈`: prepare.op is the maximum op seen. `op>₁`: prepare.op > `op_prepare_max`. `op>₂`: header.op > `op_prepare_max`.
- `✓∑`: header.checksum == prepare.checksum. `op=` / `op<`: compare ops. `view`: same view.
- `0`/`1`: must be false/true. `_`: ignored. `a0`/`a1`: *asserted* false/true; the replica crashes if violated ("impossible state").

Decisions (`RecoveryDecision`, `journal.zig:2353–2369`):
- **eql:** header and prepare match. Nothing to do.
- **nil:** legitimately empty slot.
- **fix:** the prepare is intact; rewrite the redundant header from it.
- **vsr:** mark dirty **and faulty**; repair from another replica with `get_prepare`. For a one-replica cluster this means the replica can't recover safely and aborts.
- **cut:** the entry belongs past `op_prepare_max` (the next checkpoint); truncate it so the checkpoint can be replayed.
- **cut_torn:** not in the table; assigned afterwards by `torn_prepares` (§3.6).
- **unr:** unreachable.

How to read the important rows:
- **@P** is the normal case and **@L** a normal empty slot.
- **@C**: valid redundant header, corrupt prepare. The header was written *after* the prepare, so the prepare was fully written once. It is now lost → **vsr** (faulty).
- **@K**: valid redundant header for an op, but the prepare slot reads back as reserved. A lost or misdirected write → **vsr**.
- **@F / @G**: crashed after the prepare write but before the header write → **fix** locally.
- **@M / @N**: both valid, different ops. Pick the higher op. If the prepare is higher, the log wrapped and the header write was lost → **fix**. If the header is higher, the prepare is stale → **vsr**.
- **@O**: same op, different views. Recovery can't tell which is newer (a view change may have rewritten the slot), so **vsr** even though a valid prepare is present.

The table is checked to be exhaustive: `recovery_case` asserts that exactly one case matches every combination (`journal.zig:2430–2476`), and the test `"recovery_cases"` (`journal.zig:2506`) enumerates all combinations.

> ⚠️ **Docs vs code (minor).** The older comment copy of the table (`journal.zig:1204–1239`) has a legend that omits `cut` and doesn't mention `cut_torn`; it also leaves the `R=1` decision blank for @H–@J where the code says `unr`. The code table at `journal.zig:2302` is authoritative. Also, `config.zig:175–178` refers to `recover_torn_prepare()`; the function is now `torn_prepares()`.

### 3.6 Refining torn writes: `torn_prepares`

Cases @B and @C both say "corrupt prepare". A torn write and a corruption look identical at the slot level. `torn_prepares` (`journal.zig:1359`) uses the *whole log* to separate them:

1. Compute `op_max`, the highest op in any valid prepare or redundant header.
2. Only slots in `(op_max, op_prepare_max]` are candidates. These are past the head, so if something there is corrupt, it was most likely being appended when we crashed.
3. A candidate must have a valid redundant header from a **previous wrap** (`op <= op_checkpoint`) and an invalid prepare.
4. If any *other* kind of fault sits in that range (a corrupt redundant header, or a reserved one), `op_max` itself is uncertain, since the faulty slot could hold the true head. Return nothing.
5. At most `journal_iops_write_max` (32) torn slots are accepted, because that's the most prepare writes that can be in flight at once. More than that means something else is going on: truncate nothing.

Those slots become `cut_torn`: reset to reserved, with dirty and faulty cleared (`journal.zig:1543–1550`). This lets the replica start normally instead of in `recovering_head`.

If the head op still can't be determined, the replica enters `Status.recovering_head`: "it might have forgotten some of the messages it has sent or received before. Instead, it waits for a View message" (`replica.zig:51–56`). It doesn't vote in view changes until a primary hands it a trustworthy head.

### 3.7 The counter-example in ARCHITECTURE.md

Quoted from "Protocol Aware Recovery":

> The primary accepts a request from the client, converts it to a prepare by assigning it a specific operation number and starts the replication procedure. During replication, the primary successfully appends the prepare to its local WAL but fails to broadcast it to other replicas. Now, for whatever reason, the primary restarts, the cluster switches to a different primary and the prepare gets corrupted in the original primary's WAL. As the request was prepared, the prepare should make it into the new view. But because there's only one copy of prepare in the cluster, and it got corrupted, it is impossible to execute this prepare.

As a timeline with 6 replicas:

```
 R0 (old primary)  R1   R2   R3   R4   R5
 append op 50 ✓     -    -    -    -    -      (broadcast never left R0)
 crash/restart
 op 50 corrupted → recovery marks slot faulty (@C), CAN'T nack
 view change → JV messages
                   nack nack nack nack nack    ← each never had op 50
```

Op 50 was *possibly* committed in the sense that the protocol can't rule it out from R0's view alone. But it exists in exactly one copy, and that copy is gone. Only the other replicas can prove it never reached a replication quorum.

### 3.8 NACKs during a view change

When a replica joins a view change it sends a `join_view` (JV; VRR's DoViewChange). The body has its suffix of headers. The header carries two `u128` bitsets, one bit per header (`replica.zig:8833–8915`):

- **present_bitset:** "I have this exact prepare and can send it to you."
- **nack_bitset:** "I definitely never prepared this." Set when:
  1. "We don't have a prepare at all, and that's not due to a fault" (no header, not faulty).
  2. "We have this header in memory, but haven't persisted it to disk yet" (dirty, not faulty), so we never acked it.
  3. "We have a _different_ prepare — safe to nack even if it is faulty."

A faulty slot with the same op never nacks. That's the whole point of §3.4.

The new primary runs `quorum_headers` (`replica.zig:11846`). For each op from `commit_max` upwards:

```
for op in commit_max ..= op_head_max:
    canonical = header for op from a JV with the highest log_view
    nacks  = explicit nack bits
           + implicit nacks (JV too short, different header, or header
             already truncated in a newer log_view)
    copies = JVs that have the canonical prepare present

    if nacks >= quorum_nack_prepare:  truncate op and everything after it
    elif no canonical header or copies == 0:
        if not all replicas have sent JVs: awaiting_repair   (wait for more JVs)
        else:                              complete_invalid  (cannot form a view)
    else: op survives into the new view
```

`op_head_min` (commit_max) is never nacked (`replica.zig:11962`): it is known committed.

### 3.9 The nack quorum

From `vsr.quorums` (`vsr.zig:1244–1247`):

```zig
// We need to have enough nacks to guarantee that `quorum_replication` was not reached,
// because if the replication quorum was reached, then it may have been committed.
const quorum_nack_prepare = replica_count - quorum_replication + 1;
assert(quorum_nack_prepare + quorum_replication > replica_count);
```

If `quorum_nack_prepare` replicas say "never had it", at most `replica_count - quorum_nack_prepare = quorum_replication - 1` replicas could have it. That's one short of a replication quorum, so it was never committed. With `quorum_replication_max = 3` (`config.zig:157`), the table in `vsr.md` "Quorums" matches the code:

| Replicas | 1 | 2 | 3 | 4 | 5 | 6 |
|---:|--:|--:|--:|--:|--:|--:|
| Replication quorum | 1 | 2 | 2 | 2 | 3 | 3 |
| View-change quorum | 1 | 2 | 2 | 3 | 3 | 4 |
| Nack quorum | 1 | **1** | 2 | 3 | 3 | 4 |

For 2 replicas the replication quorum is forced to 2 "to improve durability", so a single nack proves the other copy was never acked by both.

Back to the counter-example: the new primary needs 4 JVs to start. If R0 is among them, only 3 nacks arrive (R0 can't nack its faulty slot): `3 < 4`, so the result is `awaiting_repair`. When a fifth JV arrives, 4 nacks → op 50 is truncated. The cluster stays available, and correctly so: op 50 was never committed.

### 3.10 `get_prepare` never nacks

During WAL repair (Concept 25), a replica asks peers for a specific prepare. A peer that has a corrupt copy just **doesn't reply**. `vsr.md`: "Per PAR's CTRL Protocol, we do not nack corrupt entries, since they _might_ be the prepare being requested."

> ⚠️ **Docs vs code.** The doc comment on `on_get_prepare` (`replica.zig:3044–3047`) says "If the requested prepare has *not* been guaranteed by this replica, then send a nack." The function body (`replica.zig:3053–3121`) never sends a nack; it either sends the prepare or logs and returns. Nacks exist only as `nack_bitset` in `join_view`. `vsr.md` ("Otherwise do not reply") matches the code. The code wins.

## 4. What it means for ashi

**This is not optional.** Without it, ashi's durability claim reduces to "no disk ever corrupts an acked entry". Every concept here maps cleanly onto Rust.

**Rust features that help:**
- **Exhaustive `match`.** TigerBeetle checks its table at runtime and in a unit test. In Rust, encode the 11 facts as a tuple or a small struct of `bool`s and `match` on it. The compiler then proves every combination is handled. Use `unreachable!()` for the `a0`/`a1` "impossible" arms. Keep the table *as data* too, for logging the case label.
- **Fixed-layout headers.** `#[repr(C, align(16))] struct Header { .. }` with `const _: () = assert!(size_of::<Header>() == 256);` mirrors `message_header.zig:72`. Cast with `zerocopy` (`FromBytes`, `IntoBytes`) or `bytemuck::Pod`, both proc-macro-checked for padding. Or hand-roll it with `unsafe` plus static asserts, keeping TigerBeetle's zero-dependency stance (Concept 13).
- **Bitsets.** `[u64; 1024 / 64]` is trivial to hand-roll. `fixedbitset` or `bitvec` work if you accept a dependency.

**Patterns to copy:**
- Two on-disk rings; prepare first, redundant header second; ack only after both. With io_uring, use a linked SQE (`IOSQE_IO_LINK`) or just wait for the first completion before submitting the second. Never rely on the kernel to keep write order without a barrier.
- Write invalidated headers for faulty slots so the "faulty" state survives a crash.
- A `recovering_head` state that refuses to vote.
- Nack bits computed from `(present, dirty, faulty)` with "faulty never nacks" as a hard invariant. Assert it.

**Pitfalls specific to Rust:**
- **Enums from disk bytes.** Transmuting an unchecked byte into a Rust `enum` is undefined behaviour. Read `command`/`operation` as `u8`/`u16` and convert with `TryFrom`. `header_ok` validates the checksum *before* touching fields for the same reason (`journal.zig:2489–2491`).
- **`std::fs` and fsync.** `File::sync_all` hides the fsyncgate problem (Concept 14). Use `O_DIRECT` with `#[repr(align(4096))]` buffers.
- **Async runtimes.** A `tokio` file write can reorder relative to another unless you sequence the futures yourself. An explicit state machine (Concept 12) makes the "prepare, then header" order visible.

**Testing:** build a storage trait with a fault-injecting implementation from day one: torn writes, bit flips, misdirected writes, lost writes. TigerBeetle's simulator corrupts storage deliberately (Concept 29). Without it, the decision table is untested.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt** the mechanism (two rings, write order, faulty-never-nacks, nack quorum = n − q_rep + 1). **Adapt** the decision table into a compiler-checked Rust `match`.

## 5. Drill questions

### Q1. Recall

In what order does TigerBeetle write a prepare and its redundant header? Name the three functions in that chain, and say when the backup sends `prepare_ok`.

> _Your answer:_

### Q2. Mechanism

Explain why a replica whose slot for op X is **faulty** must not set the nack bit for op X, but a replica holding a **different** prepare for op X may nack it even if that slot is faulty.

> _Your answer:_

### Q3. Back-of-envelope

(a) For a 5-replica cluster, compute the replication, view-change and nack quorums from the formulas in `vsr.quorums`.
(b) In the ARCHITECTURE.md counter-example with 5 replicas, the old primary holds the only (now corrupt) copy and is part of the first view-change quorum. How many nacks does the new primary see from that first quorum, and does it truncate or wait?
(c) How many 4 KiB sectors make up the default redundant-header ring, and why must that exceed `journal_iops_write_max`?

> _Your answer:_

### Q4. What breaks if…

…TigerBeetle wrote the redundant header **before** the prepare (or in the same write)? Walk through a crash and say which recovery case becomes ambiguous.

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle aborts a single-replica cluster on a `vsr` decision, since there is nobody to repair from. Should ashi support single-node deployments at all? If yes, what would you add (e.g. a mirrored WAL on a second disk) to keep @C recoverable? What does that cost?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** `write_prepare` writes the sector-padded prepare into its slot of the prepares ring. Its completion runs `write_prepare_header`, which copies the header into `headers_redundant[slot]` and writes the 4 KiB headers-ring sector that contains it. That completion runs `write_prepare_on_write_header`, which clears the slot's dirty and faulty bits. A backup sends `prepare_ok` only after the prepare is written to the WAL (`vsr.md`, Normal step 4). So a valid redundant header for op X implies prepare X was once fully on disk.

**Q2.** Faulty means "I wrote this and lost it". The replica may have sent `prepare_ok` for it, so it may be one of the replicas that made op X committed. If it nacked, the nack count could reach `quorum_nack_prepare` and truncate a committed op: data loss. A replica that holds a *different* prepare for op X (a different checksum for the same op) has positive evidence that it never prepared *this* header, so it never acked it. Whether that other prepare is readable doesn't change that (nack case 3, `replica.zig:8861–8865`).

**Q3.** (a) n=5: `quorum_replication = min(3, ceil(5/2)) = 3`; `quorum_view_change = 5 − 3 + 1 = 3`; `quorum_nack_prepare = 5 − 3 + 1 = 3`. (b) The first quorum is 3 JVs, including the old primary. The old primary can't nack (faulty), so there are 2 nacks; 2 < 3 and copies = 0 → `awaiting_repair`. Once a 4th JV arrives, 3 nacks ≥ 3 → truncate. If *all* replicas have answered and it is still undecided, the result is `complete_invalid`. (c) 1024 × 256 B = 256 KiB = 64 sectors; `journal_iops_write_max = 32`. If every header sector could be in flight at once, one crash could tear all of them. Then no slot would have a trustworthy redundant header, and every corrupt prepare would look like possible corruption (`vsr`), not a torn write. Recovery would be stuck in `recovering_head`.

**Q4.** If the header goes first and the replica crashes before the prepare is fully written, then on restart the slot shows a valid redundant header *for this op* and a corrupt prepare. That's case @C, which means "fully written once, then lost". So a pure crash looks like corruption: the slot is marked faulty, the replica won't nack, and the view change may stall waiting for a copy that never existed. With a single write, a torn write can leave a valid header sector and a torn body, with the same confusion. The write order is what makes the redundant header mean "the prepare is durable".

**Q5.** Open-ended. Points to weigh: TigerBeetle's answer is "single replica = no corruption tolerance; abort". A mirrored WAL (two devices, or two copies on one device at different offsets) restores a second source for @C. But it doubles WAL write bandwidth, and correlated faults (same firmware, same controller) can defeat it. For a ledger the honest answer is likely: support single-node only for development, and require ≥3 replicas in production, as TigerBeetle recommends 6.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
