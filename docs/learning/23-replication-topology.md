# Concept 23: Replication topology

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Mechanical Sympathy" (lines 343–346), "Embracing Concurrency" (436–458), "Star Replication" (616–625)
- `../tigerbeetle/docs/internals/vsr.md`, "Protocol: Normal" (lines 67–104), "Protocol: Repair WAL" (201–221)
- `../tigerbeetle/docs/concepts/safety.md`, "High Availability" (lines 46–58)
- `../tigerbeetle/src/vsr/replica.zig`: `on_prepare` doc comment and body (lines 1997–2238), `on_prepare_timeout` (3629–3697), `on_get_prepare` (3053), `standby()` (6946–6952), `jump_to_newer_op_in_normal_status` (6957), repair window (7656–7665), `repair_prepare` (8341), `replicate` (8555–8583), `send_message_to_other_replicas_and_standbys` (9141–9148), `execute_op_reply_to_client` (5343–5366)
- `../tigerbeetle/src/vsr/client.zig`: `send_request_with_hedging` (lines 736–750)
- `../tigerbeetle/src/vsr/repair_budget.zig` (`RepairBudgetJournal`)
- `../tigerbeetle/src/vsr.zig:1302-1307` (`Members` comment)
- `../tigerbeetle/src/tigerbeetle/cli.zig` (lines 96–97, 1088–1092)
- Git history of the clone (read-only `git log`/`git show`): `132d05194` (2025-03-26, "vsr: adaptive routing…"), `ca9b69b01` (2025-08-25, "docs: explain adaptive routing"), `3aa232f95` (2026-04-26, "Remove ARR for star replication"), `75976ef3f` (2026-04-26, "docs: document star replication"), `4e275db15` (2026-05-08, "Fan out VSR prepare timeout retries")

## 1. The problem

Concept 21 says *what* the primary sends: a prepare, to be acknowledged by a replication quorum. This lesson is about *how* it travels. Who sends each copy, in parallel or hop by hop?

The choice trades three things against each other:

- **Bandwidth at the primary.** Every copy the primary sends leaves through its one network card. If it sends 5 copies of a 1 MiB prepare, its egress is 5× everyone else's.
- **Latency to quorum.** A transfer is committed only when the 3rd `prepare_ok` arrives (6 replicas, Concept 22). Every extra hop on the way adds a one-way network delay.
- **Tolerance of slow or dead replicas.** "Gray failure" (a replica that is slow but not dead) is common. A topology where one slow replica sits on everyone's path makes the whole cluster slow.

For a ledger, latency is user-visible (every payment waits for commit), and the topology must not stall when one machine misbehaves.

## 2. The obvious solution, and why it fails

**Obvious answer 1: broadcast.** The primary sends every prepare to every backup at once.

The textbook objection is bandwidth. Numbers from TigerBeetle's own constants:

```
message_size_max          = 1 MiB          (src/config.zig:159)
transfers per prepare     ≈ (1 MiB − 256 B header) / 128 B = 8,190
target throughput         = 1,000,000 transfers/s  → ≈ 122 prepares/s
one copy of the stream    ≈ 122 MiB/s ≈ 1.0 Gbit/s
broadcast to 5 backups    ≈ 610 MiB/s ≈ 5.1 Gbit/s leaving the primary
```

Every other node receives about 1 Gbit/s. The primary sends 5. "If one node in the network requires much higher bandwidth than the rest, the network will be underutilized" (ARCHITECTURE.md lines 343–344).

**Obvious answer 2: a chain (ring).** The primary sends to R1, R1 forwards to R2, and so on. Every node sends and receives about one copy. Bandwidth is perfectly balanced. But:

```
Ring, 6 replicas, quorum 3 = primary + R1 + R2, one-way delay d:
  P ─d─▶ R1 ─d─▶ R2 ─d─▶ (ack to P)        → quorum after 3d
  If R1 is slow (gray failure), R2 waits too: one slow node delays every op.
  If R1 is dead, R2 never gets the prepare from the chain at all.
```

Chains are bandwidth-optimal and latency-poor, and they are fragile. Neither obvious answer is free. TigerBeetle has used three topologies over time, which is why its docs disagree.

## 3. What TigerBeetle does

> ⚠️ **Docs vs code (major).** The docs describe three different topologies:
> - `ARCHITECTURE.md` "Mechanical Sympathy" (lines 343–346): "the primary doesn't broadcast prepares to all backups. Instead, it sends each prepare to just two backups, relying on the backups to forward it further." This describes **adaptive replication routing (ARR)**. The docs for it were added in `ca9b69b01` (2025-08-25), and ARR was removed in `3aa232f95` (2026-04-26).
> - `vsr.md` "Protocol: Normal" (lines 73–75) and its diagram: "Each replica (in a chain beginning with the primary)… Forward the prepare to the next replica in the chain." This describes the original **ring**. The text dates to at least 2023 (`a4915b30b`).
> - `ARCHITECTURE.md` "Star Replication" (lines 616–625): "the primary sends each prepare in parallel to all other replicas." This one is **current**. It was added in `75976ef3f` on the same day star became the default.
>
> **The code does star replication.** `replicate()` sends only when `self.status == .normal and self.primary()`, to all other replicas and standbys (`replica.zig:8580-8582`). Backups never forward. Also stale: `docs/concepts/safety.md` line 56 ("TigerBeetle automatically adjusts replication topology"), an ARR-era sentence, and the comment on `vsr.Members` (`src/vsr.zig:1306`): "Order determines ring topology for replication."

### 3.1 What the code does today: star

The design rationale sits right above `on_prepare`:

```zig
// src/vsr/replica.zig:1997-2013
/// Replication is simple, with a single code path for the primary and backups.
///
/// The primary starts by sending a prepare message to itself.
///
/// The primary broadcasts this prepare message to every other replica and standby, in
/// parallel to writing to its own journal.
///
/// Star replication prioritizes latency: with Flexible Paxos the primary can commit after
/// the fastest prepare_ok quorum returns, without waiting for a ring of forwards.
///
/// At the same time, asynchronous replication keeps going, so that if our local disk is
/// slow, then any latency spike will be masked by more remote prepare_ok messages as they
/// come in. This gives automatic tail latency tolerance for storage latency spikes.
///
/// The remaining problem then is tail latency tolerance for network latency spikes. If
/// prepare_oks do not arrive in time, the primary's prepare timeout fires and resends to
/// all replicas still missing from the prepare's ack set.
fn on_prepare(self: *Replica, message: *Message.Prepare) void {
```

Every replica, primary included, runs `on_prepare` for a new prepare. It calls `replicate()` for any prepare it doesn't already have (lines 2031–2050). `replicate()` decides who actually sends:

```zig
// src/vsr/replica.zig:8555-8583 (abridged)
fn replicate(self: *Replica, message: *Message.Prepare) void {
    // But each prepare should be replicated at most once, to avoid feedback loops.
    assert(!self.journal.has_prepare(message.header));
    assert(message.header.op > self.commit_min);
    …
    if (self.status == .normal and self.primary()) {
        self.send_message_to_other_replicas_and_standbys(message.base());
    }
}

// src/vsr/replica.zig:9141-9148
fn send_message_to_other_replicas_and_standbys(self: *Replica, message: *Message) void {
    for (0..self.node_count) |replica_usize| {
        const replica: u8 = @intCast(replica_usize);
        if (replica != self.replica) self.send_message_to_replica_base(replica, message);
    }
}
```

`node_count = replica_count + standby_count`, so standbys get the prepare straight from the primary too.

The comment block inside `on_prepare` (lines 2031–2037: "Replicate anything that remote replicas are likely missing… Replicate anything that we didn't previously have ourselves") can read as if backups forward. They don't. The call site is shared by design ("a single code path for the primary and backups"), and the gate inside `replicate()` makes only a `normal` primary send. That gate also covers a primary that receives an old prepare it was missing, e.g. while repairing after a view change: it rebroadcasts it. In the ring era the same call made *every* replica forward to its successor. `4e275db15` rewrote these comments for star.

### 3.2 The three topologies, side by side

```
 RING (until 2025-03; even ops clockwise, odd ops counter-clockwise)
        R1 ─▶ R2 ─▶ R3 ─▶ R4 ─▶ R5 ─┐
        ▲                          │ (standbys formed their own ring after the last replica)
        P ◀──────────── acks go straight to P ──┘

 ARR, "adaptive replication routing" (2025-03 → 2026-04)
        route is V-shaped, primary in the middle:   R5 ◀─ R4 ◀─ R2 ◀─ P ─▶ R1 ─▶ R3
        P sends to 2 neighbours; each backup forwards to at most 1 more.
        Standbys hung off one end of the route, in a static ring of their own.
        Route chosen by explore/exploit on measured prepare_ok latency.

 STAR (default since 2026-04-26)
                    R1
                    ▲
            R5 ◀─── P ───▶ R2          P sends every prepare to all 5 backups
                   ╱ ╲                 (and all standbys) in parallel.
                 R4   R3               Acks go straight back to P.
```

Sources for the old two (read via `git show`):
- **Ring:** `replicate_direction`: "Even ops replicate clockwise. Odd ops replicate counter-clockwise. This means that if the first backup after the primary is down, replication doesn't necessarily need to wait for prepare_timeout, since the next prepare (routed backwards) could trigger repair in the other replicas" (`replica.zig` at `132d05194^`, lines 7900–7909).
- **ARR:** `src/vsr/routing.zig` header (at `3aa232f95^`): "The replication route is V-shaped. Primary is in the middle, and it tosses each prepare at two of its neighbors. Backups forward prepares to at most one further backup… If any single node fails, the primary still gets a replication quorum." The primary broadcast the active route in `ping` messages. Experiments were scheduled by a PRNG with a seed shared by all replicas, "so no coordination is needed to launch an experiment!"

### 3.3 Trade-offs

Six replicas, quorum 3, one-way delay `d`, assuming the primary's own WAL write is fast:

| | Ring | ARR (V-shape) | Star |
|---|---|---|---|
| Copies sent by primary per prepare | 1 | 2 | 5 (+ standbys) |
| Primary egress at 1M TPS | ≈ 1 Gbit/s | ≈ 2 Gbit/s | ≈ 5.1 Gbit/s |
| Time to 3rd ack (all healthy) | ≈ 3d (P→R1→R2, ack) | ≈ 2d (both neighbours are 1 hop) | ≈ 2d to the **2nd-fastest of 5** |
| One slow backup on the path | delays every op routed through it | delays ops until the route adapts | masked: the primary uses the other 4 |
| One dead backup | forwarding stops; repair or timeout | still a quorum, by design | no effect beyond losing a candidate |
| Moving parts | direction by op parity | latency tracking, route experiments, route broadcast in pings | a for-loop |

The commit that made star the default (`3aa232f95`) gives the reasons:
- **Cross-region jitter.** ARR "had a few optimistic assumptions that hold in single-region deployments but get challenged in cross-region deployments… Latency in cross-region deployments is quite unreliable and jittery, so one benefits more from having more parallel requests." And "experimenting with routes on the hot path leads to latency spikes."
- **Bandwidth is not the bottleneck.** "Replication happens at the logical level… On the storage level, those 128-byte requests generate ~1000 bytes of data (~6x) due to multiple secondary indexes, and then LSM WAF adds another 20-30x bandwidth amplification. So we generate much more bandwidth on the storage side than on the replication side."
- **Tail latency.** "We have more parallel requests (e.g., 5) and wait on only 2, so we can use the 2 fastest and always 'ignore' the 3 slowest."
- **Debuggability.** "With ARR it's very hard to attribute performance issues to ARR or not… Star replication is significantly easier and more predictable."

This connects to Concept 22. Flexible quorums make the replication quorum small (3 of 6), and star makes the primary *choose* the fastest 3. Together, the three slowest replicas, or the slowest disks, never set the commit latency.

The cost is real but bounded. ~5 Gbit/s of primary egress at the 1M TPS design target fits a 10 or 25 GbE NIC. The earlier ARCHITECTURE.md concern (lines 343–346) is valid physics; TigerBeetle judged it the smaller problem.

### 3.4 Retries fan out too

If acks are late, `prepare_timeout` (250 ms, `replica.zig:1362`) fires on the primary. It resends the oldest unacknowledged prepare to **every** replica missing from that prepare's ack set, iterating from `self.replica + 1` around the ring of indices (`on_prepare_timeout`, `replica.zig:3648-3696`). Before `4e275db15` ("Fan out VSR prepare timeout retries", 2026-05-08), each timeout resent to **one** waiting replica, cycling through them: `waiting[self.prepare_timeout.attempts % waiting_count]`. That commit also deleted the ring's rationale from the `on_prepare` comment: "This keeps the primary's outgoing bandwidth limited (one-for-one) to incoming bandwidth… It is ideal if this synchronous replication to one remote replica is to the next replica, since that is the replica next in line to be primary." The timeout backs off with jitter from the replica-local PRNG (`prepare_timeout.backoff(&self.prng)`, line 3685).

### 3.5 How backups catch up: repair, not forwarding

Backups don't forward, so a backup that misses a prepare (a dropped message, a restart) must **pull** it. Concept 25 covers repair in depth. The topology-relevant parts:

- **Noticing a gap.** A prepare for `op > self.op + 1` triggers `jump_to_newer_op_in_normal_status` (`replica.zig:6957`), leaving a hole in the journal. Hash chaining (`parent`) shows which header belongs in the hole (Concept 15).
- **Waiting a little.** Ops within one pipeline of the head might still arrive normally: "Missing prepares/headers within a pipeline of ops from the head may arrive via normal replication, so wait for them instead of eagerly repairing" (`replica.zig:7662-7665`).
- **Asking a peer.** `repair_prepare` (line 8341) sends `get_prepare` to a replica chosen by `RepairBudgetJournal` (`repair_budget.zig`). It tracks an EWMA of each peer's repair latency, usually picks the fastest peer with budget, and with probability `ratio(1, 10)` tries a random one, "to try alternative repair routes". It allows at most 2 `get_prepare`s in flight per remote replica, so as not to overflow the peer's 4-message send queue.
- **Any replica can serve.** `on_get_prepare` (line 3053) answers from its WAL or pipeline cache. It stays silent if its copy is corrupt: "we do not nack corrupt entries, since they _might_ be the prepare being requested" (`vsr.md` lines 213–218, Concept 24).

So repair is a *mesh*: any replica to any replica. Normal replication is a *star*: primary to everyone. Repair traffic is pulled, budgeted and rare. Normal traffic is pushed and constant.

### 3.6 The client-facing hop is hedged too

The same "send to more than one" idea appears at both ends of a request:

- **Request:** "In addition to the primary, each request is also sent to a randomly chosen backup, to handle the case where the client → primary link is down" (`src/vsr/client.zig:736-750`). A backup that receives a new request forwards it to the primary (`replica.zig:6435-6440`).
- **Reply:** besides the primary, one backup chosen by `PRNG.from_seed(op)` also replies, so every replica agrees on which one without a message (`replica.zig:5343-5366`, Concept 20).

```
            request (hedged)                 reply (hedged)
 Client ──▶ P            ┐          P ──────────▶ Client
        └─▶ Rk ──fwd──▶ P┘          R(seed=op) ──▶ Client
```

### 3.7 Standbys in the topology

Standbys (index `≥ replica_count`) receive prepares and commit heartbeats from the primary in the same star loop. They never send `prepare_ok` (`replica.zig:8702`), so they never affect commit latency. In the ring era they formed a separate ring hanging off the last active replica. Under star that special case is gone. Standbys repair like anyone else, and `choose_any_other_replica` lets a standby pick any active replica (`replica.zig:4296-4303`).

### 3.8 Leftovers to know about

- The CLI flag `--replicate-star` is now a no-op that logs "--replicate-star is deprecated; star replication is now the default." (`src/tigerbeetle/cli.zig:96-97`, `1088-1092`).
- `replicate()` still asserts "each prepare should be replicated at most once, to avoid feedback loops" (line 8564). That protected against forwarding storms in the ring/ARR design.

## 4. What it means for ashi

**Start with star.** It is the simplest topology and the one TigerBeetle settled on after trying the alternatives. One loop over peers, with acks counted against a flexible quorum:

```rust
fn replicate(&mut self, prepare: &MessageRef) {
    debug_assert!(self.status == Status::Normal && self.is_primary());
    for node in 0..self.node_count {
        if node != self.index { self.bus.send(node, prepare.clone()); } // refcount, no copy
    }
}
```

**Rust features that help:**
- Reference-counted, pool-allocated messages (`Rc<Message>` from a fixed pool, or an index into a slab plus a refcount) let one 1 MiB buffer go to 5 sockets with no copies. TigerBeetle's `message_bus.unref` is the same idea.
- With io_uring, the 5 sends can go in as linked or batched SQEs in one `io_uring_enter` (Concept 12).

**Patterns:**
- Keep the send loop behind a small `fn targets(op) -> impl Iterator<Item = u8>`, so a topology change is local and simulator-testable. TigerBeetle switched topologies three times; the code shape made that cheap.
- Resend on timeout to *all* missing replicas, with jittered backoff.
- Make repair pull-based and budgeted per peer (small in-flight caps, latency EWMA, a 10% exploration rate).

**Crates worth evaluating** (TigerBeetle has zero dependencies; Concept 13):
- `io-uring` (low-level, close to TigerBeetle's approach) vs `tokio-uring`/`glommio` (bring a runtime and its scheduling nondeterminism). For the replica core, the low-level crate fits better.
- No "replication topology" crate is worth taking on.

**Pitfalls specific to Rust:**
- `Vec<u8>` clones per destination quietly turn 1 MiB × 5 into 5 MiB of `memcpy` per prepare. Use shared buffers.
- A per-peer `tokio::spawn` for sending loses ordering guarantees between prepares to the same peer and adds allocation on the hot path. Use one ordered send queue per connection, bounded like TigerBeetle's 4-message queue.
- Bounded channels that block the replica thread when a slow peer's queue is full turn a slow peer into a slow primary. Drop instead (the protocol tolerates loss, Concept 28) and let repair fix it.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt** star. Replication bandwidth is small next to LSM write amplification, and star's "fastest quorum wins" gives tail-latency tolerance with almost no code. Revisit only if ashi targets bandwidth-starved links.

## 5. Drill questions

### Q1. Recall

Name the three replication topologies TigerBeetle has used, in order, and say which doc passage describes each. Which one does `replica.zig` implement today, and what single condition in `replicate()` proves it?

> _Your answer:_

### Q2. Mechanism

Explain why star replication plus a replication quorum of 3 out of 6 tolerates one replica with a slow disk with no effect on commit latency. Why would the same slow disk hurt under the ring topology?

> _Your answer:_

### Q3. Back-of-envelope

At 1,000,000 transfers/s with full 1 MiB prepares, compute the primary's egress for ring, ARR and star with 6 replicas and 2 standbys. Then use the commit message's estimate (≈ 1,000 bytes of index data per 128-byte transfer, then 20–30× LSM write amplification) to estimate each replica's disk write bandwidth. Which is bigger?

> _Your answer:_

### Q4. What breaks if…

…someone "optimizes" star by having backups also forward each prepare to all other replicas, so that a prepare the primary failed to send to R3 still reaches it? Think about message counts, the `assert(!self.journal.has_prepare(...))` in `replicate()`, and what already covers that case.

> _Your answer:_

### Q5. ashi design (open-ended)

ashi might run across regions where the primary's uplink is metered and expensive. Design a topology that keeps star's tail-latency tolerance but cuts the primary's egress. What would you measure in the simulator to decide whether it beats plain star?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- (1) Ring: `vsr.md` "Protocol: Normal" ("in a chain beginning with the primary"). Even ops went clockwise, odd ops counter-clockwise.
- (2) ARR, V-shaped adaptive routing: `ARCHITECTURE.md` "Mechanical Sympathy" ("sends each prepare to just two backups, relying on the backups to forward it further") and `safety.md` ("automatically adjusts replication topology").
- (3) Star: `ARCHITECTURE.md` "Star Replication".
- Code today: star. `replicate()` sends only `if (self.status == .normal and self.primary())`, to `send_message_to_other_replicas_and_standbys`. A backup never sends a prepare onward.

**Q2.**
- The primary sends to all 5 backups at once and commits on the first 3 `prepare_ok`s, its own included if it arrives.
- A slow disk on R3 only delays R3's ack, and the primary uses the others. If the slow disk is the *primary's*, the 3 fastest remote acks still form a quorum ("any latency spike will be masked by more remote prepare_ok messages").
- In TigerBeetle's ring, a replica forwarded in `on_prepare`, *before* its own WAL write. So a slow disk on R1 didn't delay R2's copy. But the quorum for an even op was effectively P + R1 + R2, the first two replicas on the chain. R1's slow WAL write delayed its ack, and with it the commit, on every op routed that way (half of them, given the alternating directions).
- A slow *node* (CPU, NIC, gray failure) in a ring is worse: it delays the copy for everyone downstream. Star has no "downstream".

**Q3.**
- ≈ 122 prepares/s × 1 MiB ≈ 122 MiB/s per copy ≈ 1.0 Gbit/s.
- Ring: 1 copy ≈ 1 Gbit/s. ARR: 2 copies ≈ 2 Gbit/s. Star with 5 backups + 2 standbys: 7 copies ≈ 855 MiB/s ≈ 7.2 Gbit/s.
- Disk: 1M × 1,000 B ≈ 1 GB/s of logical index data. × 20–30 write amplification ≈ 20–30 GB/s of compaction IO per replica (an upper-bound estimate; real numbers depend on workload and caching).
- Storage bandwidth (20–30 GB/s) is roughly 20–35× bigger than even star's primary egress (≈ 0.9 GB/s). Even if the amplification estimate is generous, storage dominates. That is the commit message's argument for why star's extra network cost doesn't matter.

**Q4.**
- Message count goes from 5 to about 5 + 5×4 = 25 sends per prepare, an all-to-all flood that multiplies every replica's egress by about 4 and adds no safety.
- The design rule "each prepare should be replicated at most once, to avoid feedback loops" exists because forwarding can cascade (replicas re-forwarding what they just received). The assert guards that only first receipt replicates.
- The case being "fixed" is already covered twice: the primary's `prepare_timeout` resends to every replica missing from the ack set, and a backup that sees a later op detects the gap and pulls the missing prepare with `get_prepare` from the fastest peer.
- Result: more bandwidth, more queue overflow (dropped messages), no benefit.

**Q5.** Open-ended. Points a good answer weighs:
- **Hybrid "star to a quorum, then fan out":** the primary sends to the `k` replicas it expects to be fastest (k ≥ 2 so a quorum of 3 is possible), and each of those forwards to one more. That is ARR again, with ARR's weaknesses: route mistakes and jitter, plus experiment-induced spikes.
- **Per-region relay:** the primary sends one copy per remote region, and a designated replica there fans out locally. Egress scales with regions, not replicas, but a slow relay delays its whole region. You need timeouts that fall back to direct sends.
- **Erasure-coded dissemination:** send shards instead of copies. This saves bandwidth but adds CPU and complexity on the hot path.
- **Measure in the simulator** with realistic latency distributions (cross-region jitter, gray failures, packet loss): p50/p99/p999 commit latency, primary egress bytes, repair traffic, and behaviour under one slow node and one partitioned node. Beat plain star at p99 under faults, or don't ship it, which is the bar TigerBeetle applied when removing ARR.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
