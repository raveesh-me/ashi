# Concept 21: Viewstamped Replication

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/internals/vsr.md` (whole file: Glossary, Commands, Recovery, Protocols Normal / Start-View-Change / View-Change / Request-View / Repair, Quorums)
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Overview" (lines 27–90), "Embracing Concurrency" (436–465), "Protocol Aware Recovery" (540–563)
- `../tigerbeetle/docs/concepts/safety.md`, "High Availability"
- `../tigerbeetle/src/vsr/replica.zig`: `Status` (line 43), replica fields (393–470), `tick_normal_heartbeat_fault` (1584), `on_request` (1937), `on_prepare` (1997–2238), `on_prepare_ok` (2241), `on_commit` (2393), `on_exit_view` (2546), `on_join_view` (2605), `on_view` (2756), `on_prepare_timeout` (3629), `primary_index` (6918), `primary_pipeline_prepare` (7307), `send_prepare_ok` (8645), `send_join_view` (8813), `log_view_durable` (9419–9466), `primary_set_log_from_join_view_messages` (9735), `primary_start_view_as_the_new_primary` (9872), `transition_to_view_change_status` (10197), `send_commit` (11388), `JVQuorum` (11560–11990)
- `../tigerbeetle/src/vsr/message_header.zig` (`Header`, `Prepare`, `PrepareOk`, `Commit`, `ExitView`, `JoinView`, `View`)
- `../tigerbeetle/src/vsr.zig` (`Command`, line 251; `Operation`, line 301)
- `../tigerbeetle/src/vsr/fault_detector.zig` (header comment)
- `../tigerbeetle/src/constants.zig` (lines 255–316), `../tigerbeetle/src/config.zig` (lines 120–164, 228)
- Paper (not in the clone): Liskov & Cowling, *Viewstamped Replication Revisited* (VRR), 2012

## 1. The problem

Concept 20 reduced replication to one question: how do six machines agree on **one ordered log of prepares**, even when machines crash, restart, and drop or reorder messages?

The agreement must survive the worst moment: the primary crashes halfway through sending an entry. Some replicas have it, some don't. The new leader must never drop an entry a client was told is committed, and never keep two different entries for the same slot. For a ledger, either mistake means a balance that some client saw is now different.

## 2. The obvious solution, and why it fails

**Primary/backup with a timeout.** One machine is primary. Backups copy what it sends. If the primary goes silent for 5 seconds, the backup with the most data promotes itself.

Worked failure, 3 replicas (A primary, B, C):

```
t0  A sends op 7 to B. B acks. A tells the client "committed".
t1  A is cut off from B and C, but is still alive (a partition, not a crash).
t2  C times out, sees B has op 7, so B promotes itself to primary (or C does).
t3  A, still believing it is primary, accepts op 8 from a client on its side.
    The new primary also assigns op 8, to a different request.
    → two different "op 8"s: split brain. A client saw both commit.
```

Timeouts can't tell "dead" from "slow" or "partitioned". Something must prevent the old primary from committing once a new one exists. That something is **quorum intersection** plus a **view number** that fences off old leaders. VSR, Raft and Paxos are three packagings of that idea.

## 3. What TigerBeetle does

TigerBeetle implements **Viewstamped Replication (VSR)**, following *Viewstamped Replication Revisited* (VRR) with changes listed in 3.11.

### 3.1 Terminology map

VSR, Raft and Multi-Paxos solve the same problem with different words. TigerBeetle also renamed VRR's three view-change messages "to put the state (`View`) into the spotlight, as per Fred Brooks" (`vsr.md` line 17).

| TigerBeetle (code) | VRR paper | Raft | Multi-Paxos |
|---|---|---|---|
| `view` | view-number | term | ballot |
| primary | primary | leader | distinguished proposer |
| backup | backup | follower | acceptor |
| standby | — | non-voting learner | learner |
| `op` | op-number | log index | slot / instance |
| `commit_max` | commit-number | `commitIndex` | (chosen up to) |
| `commit_min` | (executed up to) | `lastApplied` | — |
| `request` | Request | client request | client request |
| `prepare` | Prepare | `AppendEntries` | Phase 2a "accept" |
| `prepare_ok` | PrepareOK | `AppendEntries` reply | Phase 2b "accepted" |
| `commit` (heartbeat) | Commit | empty `AppendEntries` | — |
| `exit_view` (EV) | StartViewChange | PreVote / CheckQuorum (roughly) | — |
| `join_view` (JV) | DoViewChange | `RequestVote`, but it carries the log | Phase 1b "promise" with accepted values |
| `view` | StartView | new leader's first `AppendEntries` | — |
| `get_view` | GetState (roughly) | — | — |
| state sync (Concept 25) | state transfer | `InstallSnapshot` | — |

Two differences matter more than names:
- **Who leads is fixed, not elected.** In Raft, any up-to-date replica can win an election. In VSR the primary of view `v` is predetermined (3.8). So the view change moves the *log* to a known leader. Raft moves *leadership* to a replica that already has the log.
- **"Prepare" means opposite things.** Paxos "prepare" is phase 1 (leader election). VSR "prepare" is phase 2 (replicating an entry). Don't mix them up when reading papers.

### 3.2 Glossary: a replica's consensus state

All of these are fields of the replica struct in `src/vsr/replica.zig`:

- **`replica`, `replica_count`, `standby_count`, `node_count`** (lines 207–215). A cluster has up to `replicas_max = 6` active replicas and `standbys_max = 6` standbys (`constants.zig:24-26`). Index `≥ replica_count` means standby.
- **`view: u32`** (line 393). The current view, a monotonically increasing integer. Each view has one primary. A `u32` "allows for a minimum lifetime of 136 years at a rate of one view change per second" (`message_header.zig:46-47`).
- **`log_view: u32`** (line 405). "The latest view where the replica was a primary and acquired a JV quorum, or the replica was a backup and processed a View message. i.e. the latest view in which this replica changed its head message." Invariant: `view ≥ log_view`, and they are equal when `status = normal`. VRR calls this v′.
- **`status`** (line 408; enum at line 43): `normal`, `view_change`, `recovering`, `recovering_head`. `recovering_head` means the replica found its own persistent state damaged and "can not participate in consensus, as it might have forgotten some of the messages it has sent or received before" (lines 51–56).
- **`op: u64`** (line 421). The op number of the latest prepared entry: the replica's **head**.
- **`commit_min: u64`** (line 432). The latest op this replica has *executed*. It "may have to wait for repairs to complete before commit_min reaches commit_max".
- **`commit_max: u64`** (line 449). The latest op the *cluster* has committed, as far as this replica knows. "This is the commit number in terms of the VRR paper." On a normal primary, `commit_max = commit_min` and `commit_max = op - pipeline.queue.prepare_queue.count`.
- **primary / backup.** `primary()` is true when `primary_index(view) == replica` (line 6924). Every non-primary active replica is a backup.
- **standby.** "Standbys follow the cluster without participating in consensus. In particular, standbys receive and replicate prepares, but never send prepare-oks" (lines 6946–6948). They also ignore EV and JV messages. The CLI marks them "Experimental: standbys don't have a concrete practical use-case yet" (`src/tigerbeetle/cli.zig:42`).
- **solo.** A one-replica cluster (line 6941). No view changes.
- **checkpoint, superblock, journal, WAL, header.** See `vsr.md` "Glossary". A **header** identifies an entry. View changes pass headers, not whole entries.
- **pipeline** (line 467). On a normal primary, a FIFO queue of in-flight prepares plus pending requests. On everyone else, a cache of prepares.

### 3.3 The message header and the command set

Every message, every WAL entry and every grid block starts with the same 256-byte header. Reusing the network header as the on-disk header means "prepare messages from the primary can simply be journalled as is by the backups without requiring any further modification" (`message_header.zig:14-16`).

```
src/vsr/message_header.zig:17-75   Header (extern struct, 256 bytes, align 16, no padding)

offset  size  field
     0    16  checksum              ─┐ covers bytes 16..255 of the header
    16    16  checksum_padding       │ (reserved to widen to u256 later)
    32    16  checksum_body         ─┤ covers the body that follows
    48    16  checksum_body_padding  │
    64    16  nonce_reserved          (future AEAD)
    80    16  cluster                 (binds intent: wrong cluster = rejected)
    96     4  size                    (header + body)
   100     4  epoch                   (reconfiguration, future use)
   104     4  view
   108     4  release
   112     2  protocol
   114     1  command                 (enum below)
   115     1  replica                 (original author, even if forwarded)
   116    12  reserved_frame
   128   128  reserved_command        (per-command fields, e.g. Prepare's op/commit/parent)
```

The header checksum covers the rest of the header, so "the header to be trusted without having to recv() or read() the associated body" (line 20). Each command has its own typed view of the last 128 bytes (`Header.Type(command)`, line 78).

The commands (`src/vsr.zig:251-290`):

| # | Command | From → to | Purpose |
|---|---|---|---|
| 1, 2 | `ping`, `pong` | replica ↔ replica | liveness, clock sync (Concept 26) |
| 3, 4 | `ping_client`, `pong_client` | client ↔ replica | client learns the view |
| 5 | `request` | client → primary | a batch to execute |
| 6 | `prepare` | primary → replicas | a log entry |
| 7 | `prepare_ok` | replica → primary | "it's in my WAL" |
| 8 | `reply` | replica → client | result |
| 9 | `commit` | primary → backups | heartbeat carrying `commit_max` |
| 10 | `exit_view` | replica → all | "I think the primary is dead" |
| 11 | `join_view` | replica → all | my log suffix, for the new view |
| 13 | `get_view` | backup → primary | "send me the current view" |
| 14, 17 | `get_headers`, `headers` | replica ↔ replica | repair journal headers |
| 15 | `get_prepare` | replica → replica | repair a WAL entry |
| 16 | `get_reply` | replica → replica | repair a stored client reply |
| 18 | `eviction` | primary → client | session evicted (Concept 27) |
| 19, 20 | `get_blocks`, `block` | replica ↔ replica | grid repair / state sync (Concept 25) |
| 24 | `view` | primary → backups | the new view's log suffix + checkpoint |
| 12, 21, 22, 23 | `deprecated_*` | — | retired ordinals, never reused |

Prepares also carry `parent` (checksum of the previous prepare, so the log is a hash chain: Concept 15), `op`, `commit`, `timestamp`, `checkpoint_id` and `client`/`request` (`message_header.zig:688-739`).

### 3.4 Normal operation

The steps, as the code does them today (6 replicas, primary R0):

```
Client          R0 (primary)                R1      R2      R3      R4      R5
  │  request       │                           │       │       │       │       │
  ├───────────────▶│ on_request (1937)         │       │       │       │       │
  │                │  realtime_synchronized()  │       │       │       │       │
  │                │ primary_pipeline_prepare (7307):  op = op+1, timestamp,  │
  │                │  parent = checksum(op-1), commit = commit_max            │
  │                │ on_prepare → replicate → send to every replica + standby │
  │                ├──prepare──────────────────▶───────▶───────▶───────▶──────▶  (in parallel)
  │                │ append to own WAL         │ append to WAL (each)          │
  │                │◀──prepare_ok──────────────┤       │       │       │       │
  │                │◀──prepare_ok (self, after own WAL write)                  │
  │                │◀──prepare_ok──────────────────────────────┤               │
  │                │ 3 oks = quorum_replication → commit_pipeline (4309)       │
  │                │ prefetch → execute → compact (in op order)               │
  │◀───reply───────┤   (+ one PRNG-chosen backup also sends the reply)        │
  │                ├──next prepare (header.commit = n) or commit heartbeat───▶ backups
  │                │                           commit_journal (4334): execute ops ≤ commit_max
```

Step by step:

1. **Request.** The client sends to the primary of the view it knows, `view % replica_count` (`src/vsr/client.zig:741`). A backup that gets a new request forwards it to the primary (`replica.zig:6435-6440`).
2. **Prepare.** The primary assigns `op = self.op + 1`, a timestamp (Concept 20), `parent` = checksum of its current head, and `commit = commit_max` (`replica.zig:7393-7430`). It reuses the request's buffer as the prepare: "Reuse the Request message as a Prepare message by replacing the header."
3. **Replicate and write, concurrently.** The primary calls its own `on_prepare`. That sends the prepare to everyone and starts its own WAL write at the same time (`replica.zig:1997-2013`). Concept 23 covers the topology.
4. **prepare_ok.** Each replica writes the prepare to its WAL, then sends `prepare_ok` to the primary of the *current* view (`send_prepare_ok`, line 8645). Standbys return early and never ack (line 8702).
5. **Commit.** The primary counts oks per prepare. "Wait until we have a quorum of prepare_ok messages (including ourself)" (line 2286). The quorum may or may not include the primary's own ok: ARCHITECTURE.md says "This quorum doesn't need to include the primary." Prepares commit strictly in op order (3.5).
6. **Reply.** The primary replies. One backup, chosen by `PRNG.from_seed(op)`, also replies, in case the primary→client link is down (`replica.zig:5343-5366`).
7. **Backups learn the commit** from either the `commit` field of a later prepare (`advance_commit_max`, line 2101), or a periodic `commit` message (`send_commit`, line 11388; `commit_message_timeout` is 500 ms, line 1372). They then execute in order (`commit_journal`).

> ⚠️ **Docs vs code:** `vsr.md` "Protocol: Normal" step 3 says "Each replica (in a chain beginning with the primary)… Forward the prepare to the next replica in the chain", and its diagram shows R1 forwarding to R2. The current code is **star**: `replicate()` sends only when `self.status == .normal and self.primary()`, to all other replicas and standbys (`replica.zig:8580-8582`). Backups don't forward. Concept 23 has the full history. `vsr.md` also doesn't mention the extra backup reply in step 6.

**Lost oks.** If oks don't arrive, `prepare_timeout` (250 ms, line 1362) fires. The primary resends the oldest pending prepare to every replica not yet in its ack set, with backoff (`on_prepare_timeout`, lines 3629–3697).

### 3.5 Pipelining

The primary doesn't wait for op `n` to commit before preparing `n+1`. "Execution of requests has to be sequential but replication can be concurrent" (ARCHITECTURE.md line 449).

```zig
// src/constants.zig:252-260
/// The maximum number of Viewstamped Replication prepare messages that can be inflight at a time.
/// This is immutable once assigned per cluster, as replicas need to know how many operations might
/// possibly be uncommitted during a view change, and this must be constant for all replicas.
pub const pipeline_prepare_queue_max: u32 = config.cluster.pipeline_prepare_queue_max;
pub const pipeline_request_queue_max: u32 = (clients_max + 1) -| pipeline_prepare_queue_max;
```

Production values: `pipeline_prepare_queue_max = 8` (`config.zig:155`) and `clients_max = 64` (`config.zig:228`). So up to **8 prepares in flight** and `(64 + 1) − 8 = 57` requests queued behind them. Each client has at most one request in flight. The `+ 1` is the primary's own pulse/upgrade request.

```
 op:   … 1000 │ 1001 1002 1003 1004 1005 1006 1007 1008 │ 1009 … 1065
      committed│◀──── prepare_queue (≤ 8, in flight) ───▶│◀ request_queue (≤ 57) ▶
   commit_max ─┘                                    op ─┘
```

Why a hard limit of 8 matters for correctness, not just memory:
- A view change must account for every *possibly uncommitted* op. At most 8 ops past `commit_max` can be uncommitted, so a JV only needs the last ~9 headers. `view_change_headers_suffix_max = 8 + 1` (`config.zig:156`) and `view_headers_max = 9 + 2 = 11` (`constants.zig:304`).
- A JV marks each of those headers in two `u128` bitsets (`present_bitset`, `nack_bitset`, `message_header.zig:1145-1149`). Hence the assert `pipeline_prepare_queue_max + 1 <= @bitSizeOf(u128)` (`constants.zig:270-272`).
- Commit is in order. The pipeline comment: "This allows us to pipeline without the complexity of out-of-order commits" (`replica.zig:470`).

Back-of-envelope: with 1 ms between primary and its two fastest backups, one op at a time caps the cluster at ~1,000 prepares/s. Eight in flight allows ~8,000/s, far more than the single core can execute at ~8,190 transfers each (Concept 7).

### 3.6 Detecting a dead primary: ExitView

VRR starts a view change when a backup times out. TigerBeetle adds two refinements.

**Adaptive heartbeat.** Each backup runs a `FaultDetector` (`src/vsr/fault_detector.zig`). It tracks the arrival rate of prepares and commit messages, and raises an alarm on "a suspicious absence of cars, relative to recent history". An idle primary sends `commit` heartbeats so silence still means something. The primary also injects extra commits "central-bank style" so that a sudden drop in client load doesn't look like a crash. In `tick_normal_heartbeat_fault` (line 1584): `green` is fine, `yellow` waits, `red` makes a backup call `send_exit_view()`.

**Exit-view quorum.** A backup that suspects the primary broadcasts `exit_view` to everyone, itself included (`send_exit_view`, line 8792). Nobody changes view until a **view-change quorum** of EVs agrees (`on_exit_view`, lines 2546–2589):

```zig
// src/vsr/replica.zig:2553-2556
// Wait until we have a view-change quorum of messages (possibly including ourself).
// This ensures that we do not start a view-change while normal request processing
// is possible.
const threshold = self.quorum_view_change;
```

A backup that receives a `commit` clears its own EV bit (line 2430). Collected EVs expire after `exit_view_window_timeout` (5 s, line 1385). `vsr.md` explains the payoff: a replica that can send but not receive keeps shouting EV, "but if the remainder of the cluster is healthy, they will never achieve a quorum, so the view is stable." This is the role Raft's PreVote/CheckQuorum extensions play.

On the primary side, if it hasn't heard a new prepare_ok for 10 s (`primary_abdicate_timeout`, line 1367), it **abdicates**: it stops sending commit heartbeats (`send_commit`, lines 11397–11406), so backups notice and start a view change.

### 3.7 View change: EV → JV → View

Once a replica has `quorum_view_change` EVs for view `v`, it calls `transition_to_view_change_status(v + 1)` (line 10197): `status = view_change`, `view = v + 1`, and it broadcasts a JV.

```
 R1, R2, R3 … (any replica)                          P = primary_index(v+1)
 ───────────────────────────                         ──────────────────────
 FaultDetector red → exit_view(v) ──▶ all
 collect quorum_view_change EVs
 status=view_change, view=v+1
 join_view(v+1) ─────────────────────────────────▶   collect quorum_view_change JVs (incl. own)
   body: up to 11 headers, head first                │
   header: op, commit_min, checkpoint_op,            ├─ lagging a checkpoint? → forfeit:
           log_view, present_bitset, nack_bitset     │    jump to view whose primary is current
                                                     ├─ quorum_headers(): pick canonical headers,
                                                     │    truncate ops with a nack quorum
                                                     ├─ log_view := v+1
                                                     ├─ repair headers + prepares (get_headers,
                                                     │    get_prepare), commit up to commit_max
                                                     ├─ status=normal
 on_view: install suffix (+checkpoint),  ◀───────────┴─ view(v+1): suffix headers + checkpoint
 status=normal, send prepare_oks
 for uncommitted ops, repair
```

What a JV carries (`send_join_view`, lines 8813–8955):
- Headers for every possibly uncommitted op, from the head down to the replica's `commit_max` (at most `view_headers_max = 11`).
- For each header, a **present** bit ("the prepare is on disk, is being written to disk, or is cached in memory") and a **nack** bit ("definitely not prepared"). A gap is sent as a "blank" placeholder header.
- `log_view`, `op`, `commit_min`, `checkpoint_op`.

JVs go to all other replicas (line 8941). Backups use them to learn the new view; only the new primary collects a quorum.

### 3.8 Primary selection: `view mod replica_count`

```zig
// src/vsr/replica.zig:6917-6921
/// Returns the index into the configuration of the primary for a given view.
pub fn primary_index(self: *const Replica, view: u32) u8 {
    return @intCast(@mod(view, self.replica_count));
}
```

There is no vote. Every replica computes the same answer, and a client finds the primary from the view alone. If the designated replica is dead, its JV quorum never forms. Replicas time out (`view_change_status_timeout`, 5 s, line 1395), send EV again, and move to `v + 2`, whose primary is the next replica round-robin. Standbys can never be primary, because their index is `≥ replica_count`.

Two exceptions skip views on purpose:
- A **syncing** replica (Concept 25) is "not eligible to be primary". If it would be primary of `view_new_min`, it moves straight to `view_new_min + 1` (lines 10208–10217).
- A **lagging** new primary forfeits (3.9).

### 3.9 How the new primary picks the log

This is the safety-critical step. VRR's rule: take the log from the DoViewChange with the largest v′ (log_view); break ties by the largest op. TigerBeetle keeps that core and adds Protocol-Aware Recovery (Concept 24), working on headers only.

**Forfeit first.** Before installing anything, the primary checks whether it is too far behind (`on_join_view`, lines 2679–2722). It forfeits if:
- its next checkpoint is already durable on the cluster (`Checkpoint.durable(op_checkpoint_next, commit_max)`), or
- some JV has a newer checkpoint and `view − log_view < replica_count`. This is a heuristic that "gives a more-up-to-date replica *one* chance to step up as primary".

It then jumps straight to the next view whose primary sent a JV with the cluster's newest checkpoint.

> ⚠️ **Docs vs code:** `vsr.md` "Protocol: View-Change" lists two forfeit rules, both phrased "ahead of the primary by more than one checkpoint". The code forfeits when the primary lags by **one** durable checkpoint, or heuristically when any JV has a higher checkpoint. Trust the code (`replica.zig:2679-2722`).

**Canonical headers.** Several versions of "op 1005" can exist, prepared in different views. `JVQuorum` (doc comment at lines 11560–11650) defines:
- A **canonical JV** is one from the highest `log_view` in the quorum (`log_view_max`, line 11777).
- **Canonical headers** are the union of headers from canonical JVs. They "take precedence over uncanonical headers". JVs with the same `log_view` must not conflict; `JVQuorum.verify` asserts it.

**Choosing the head** (`quorum_headers`, lines 11846–11990). For each op from `op_head_min` (the highest op known committed) up to the highest canonical op:

```
for op in op_head_min ..= op_head_max:
    canonical = the canonical header for op (if any)
    copies    = JVs that have that exact header AND the present bit
    nacks     = JVs that explicitly nack op
              + JVs holding a *different* (older-view) header for op   (implicit nack)
              + JVs whose head is below op, or whose suffix doesn't reach op
    if nacks >= quorum_nack_prepare:  head = op - 1; stop       # provably uncommitted: truncate
    if canonical is None or copies == 0:
        if not all replicas answered: return awaiting_repair       # wait for more JVs
        else:                         return complete_invalid      # cluster is stuck
head = op_head_max if no truncation
```

Why a nack quorum proves "uncommitted": a commit needs `quorum_replication` copies. If `replica_count − quorum_replication + 1` replicas say "never had it", fewer than `quorum_replication` can have it (Concept 22). For 6 replicas that is 4 nacks.

Why it waits rather than guessing: an op with a canonical header but no available copy *might* be committed. Dropping it could lose a committed transfer. Keeping it with no copy is impossible. So the primary waits for more JVs (`awaiting_repair`).

Then `primary_set_log_from_join_view_messages` (line 9735):
- advances `prepare_timestamp` to the highest timestamp seen, so time never goes backwards across views;
- sets `op` to the chosen head and `commit_max` to the highest known commit;
- installs canonical headers with `replace_header`;
- trusts uncanonical headers at or below the sender's `commit_min`, and otherwise only uses them for repair.

The primary then sets `log_view = view` (line 2734), repairs missing headers and prepares, commits everything up to `commit_max`, and calls `primary_start_view_as_the_new_primary` (line 9872). That rebuilds the pipeline from the uncommitted suffix, goes `normal`, and sends prepare_oks to itself for those ops. The uncommitted suffix must win a **fresh quorum in the new view**. On the View message, backups send prepare_oks for the uncommitted ops they hold, and they always address them to the *current* primary, never the old one (`send_prepare_ok`, lines 8710–8720). Backups missing a prepare repair it with `get_prepare`.

### 3.10 Why `log_view` must never go backwards

`log_view_durable` (lines 9419–9466) has a worked example, condensed:

```
view a:  R0 prepares 1a 2a 3a; nobody else gets them.        R0 log_view=a
view b:  R1 prepares 4b 5b 6b at the same ops; nobody gets them. R1 log_view=b
view c:  R2 becomes primary, loads R1's ops → "1c 2c 3c", R0 follows.  R0 log_view=c
         (ops may commit in view c)
Now R0 crashes and its log_view regresses to a. R2 is partitioned.
view d:  JVs from R0 (log_view=a) and R1 (log_view=b).
         b > a, so R1's 4b 5b 6b are canonical and replace R0's entries,
         which may have been committed in view c.   → fork.
```

So `view` and `log_view` are written to the superblock before they are advertised. A replica must not advertise a `log_view` in a JV higher than its durable one.

### 3.11 TigerBeetle's departures from VRR

From `vsr.md` and the code:

1. **No Recovery protocol** (VRR §4.3). "Instead, replicas persist their VSR state to the superblock. This ensures that a recovering replica never backtracks to an older view." A replica that finds its head damaged enters `recovering_head` and waits for a `view` message. It uses a random nonce to tell a fresh `view` from a stale one (`on_view`, lines 2760–2773).
2. **Renamed messages:** ExitView / JoinView / View for StartViewChange / DoViewChange / StartView.
3. **ExitView runs in `normal` status too**, not only in `view_change`. A view change needs a *quorum* of suspicions, which gives liveness under asymmetric partitions (3.6). The primary can also abdicate.
4. **Headers, not logs.** JV and View carry at most 11 headers. Prepares are fetched separately with `get_prepare`, so a view change is cheap.
5. **Protocol-Aware Recovery nacks** (Concept 24). The new primary truncates only ops with a nack quorum, and never truncates on a checksum failure.
6. **Flexible quorums** (Concept 22): replication quorum 3 and view-change quorum 4 for 6 replicas.
7. **Hash-chained prepares.** `parent` links every entry, and repair walks the chain backwards (`vsr.md` "Protocol: Repair Journal").
8. **Checkpoints and state sync** instead of VRR's unbounded log and GetState. The WAL is a ring of `journal_slot_count = 1024` slots (`config.zig:158`). A replica too far behind syncs grid blocks (Concept 25), and a lagging primary forfeits.
9. **`get_view` / `view`** let a backup that sees a newer view, or can't commit, ask the primary for the current suffix and checkpoint (`vsr.md` "Protocol: Request/View").
10. **Leader-assigned timestamps** in the prepare (Concept 20), and an adaptive fault detector instead of a fixed timeout.
11. **Standbys**, extra nodes that receive prepares but never vote.

## 4. What it means for ashi

**Model each replica as an explicit state machine,** as TigerBeetle does: one `on_<command>` handler per message, a `tick()` that drives timeouts, and no `async` in the protocol core (Concept 12). In Rust:

```rust
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command { Reserved = 0, Ping = 1, Pong = 2, /* … */ Prepare = 6, PrepareOk = 7 /* … */ }

#[repr(C, align(16))]
pub struct Header {          // 256 bytes; per-command views over `reserved_command`
    checksum: u128, checksum_padding: u128,
    checksum_body: u128, checksum_body_padding: u128,
    nonce_reserved: u128, cluster: u128,
    size: u32, epoch: u32, view: u32, release: u32,
    protocol: u16, command: Command, replica: u8,
    reserved_frame: [u8; 12], reserved_command: [u8; 128],
}
const _: () = assert!(core::mem::size_of::<Header>() == 256);
```

**Rust features that help:**
- Exhaustive `match` over `Command` and `Status` catches unhandled messages at compile time.
- Newtypes (`struct View(u32)`, `struct Op(u64)`) stop you passing a view where an op belongs. TigerBeetle relies on asserts for this.
- `#[repr(u8)] enum` for `Command` is safe to *send*, but casting raw bytes into it is undefined behaviour for unknown values. Parse `u8 → Command` with `TryFrom`, and keep retired ordinals reserved.

**Patterns:**
- Keep the replica a single-writer struct driven by one event loop. Messages and timer ticks are the only inputs. That makes it simulator-friendly (Concept 29).
- Separate the pure decision logic (like `JVQuorum::quorum_headers`) into side-effect-free functions you can fuzz on their own.
- Persist `view` and `log_view` before sending any message that advertises them.
- Bound everything: pipeline depth, JV header count, EV window. Size the bitsets from the bound with a `const` assertion.

**Crates worth evaluating** (TigerBeetle has zero dependencies; Concept 13):
- `openraft` or `raft-rs` (TiKV). They are mature, but Raft-shaped: elected leaders, no nack truncation, no flexible quorums, and their own storage traits. Adopting one means giving up TigerBeetle's storage-fault model.
- `stateright` for model-checking a small protocol model alongside the real code.
- Likely conclusion: write the protocol yourself and borrow only testing tools.

**Pitfalls specific to Rust:**
- Async-trait-style message handlers (`async fn on_prepare`) make interleavings depend on the executor. Keep handlers synchronous and run-to-completion.
- `Vec<Option<Box<Message>>>` for quorum tracking allocates on the hot path. Use fixed arrays of `[Option<MessageRef>; REPLICAS_MAX]` with reference-counted pool messages, like `JVQuorumMessages`.
- `u32` view overflow: use `checked_add` and assert. Never wrap.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt** VSR with TigerBeetle's departures. The deterministic primary, header-only view change and PAR nacks fit a storage-fault-aware ledger better than stock Raft.

## 5. Drill questions

### Q1. Recall

Define `view`, `log_view`, `op`, `commit_min` and `commit_max`. State one invariant that relates each pair: (`view`, `log_view`), (`commit_min`, `commit_max`), (`op`, `commit_max`) on a normal primary.

> _Your answer:_

### Q2. Mechanism

A backup can send `exit_view` but can't receive anything (asymmetric partition). Explain step by step why it can't force a view change on an otherwise healthy 6-replica cluster. Then explain what it does when the partition heals.

> _Your answer:_

### Q3. Back-of-envelope

Production config, 64 clients all busy. (a) How many requests can sit at the primary, in total, waiting to commit? (b) How many headers can a JV carry, and how many bytes are they? (c) Why must `pipeline_prepare_queue_max + 1` fit in 128?

> _Your answer:_

### Q4. What breaks if…

…the new primary, instead of running the nack/copies logic in `quorum_headers`, simply took the JV with the highest `op` (ignoring `log_view`) and truncated anything it couldn't fetch within 1 second?

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle fixes the primary as `view mod replica_count`, so a dead replica's views are skipped one timeout at a time. Raft elects whoever is up to date. Which would you choose for ashi, and how would you bound failover time when two of six replicas are down? Consider the forfeit rule and the syncing exception.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- `view`: the view the replica is in, or is trying to enter. Each view has one primary.
- `log_view`: the latest view in which this replica's log head was set by a view change (as primary with a JV quorum, or as backup on a View message).
- `op`: the replica's head, the highest prepared op.
- `commit_min`: the highest op this replica has executed.
- `commit_max`: the highest op it knows the cluster has committed.
- Invariants: `view ≥ log_view`, with equality when `status = normal`. `commit_min ≤ commit_max`. On a normal primary, `commit_max = commit_min = op − prepare_queue.count`, so `op − commit_max ≤ pipeline_prepare_queue_max = 8`.

**Q2.**
- Its `exit_view` messages reach the others, but each healthy backup keeps receiving prepares or `commit` heartbeats. So their own `commit_fault` never goes red, they never set their own EV bit, and a received commit clears it.
- `on_exit_view` needs `quorum_view_change = 4` distinct EVs for the same view. One noisy replica, even with itself counted, is 1 of 4. Stale EVs also expire after the 5 s window.
- So the view stays stable.
- When the partition heals, if it was isolated in `normal` it simply receives commits/prepares again, may need repair (`get_prepare`, `get_headers`), and rejoins the same view. If it had moved to `view_change` for a higher view, it learns the real view via JV/View or `get_view` and rejoins (`vsr.md` "Protocol: Start-View-Change").

**Q3.**
- (a) `pipeline_prepare_queue_max + pipeline_request_queue_max = 8 + 57 = 65 = clients_max + 1`. That's 64 client requests plus one primary-originated request (pulse/upgrade).
- (b) `view_headers_max = view_change_headers_suffix_max + 2 = (8 + 1) + 2 = 11` headers × 256 bytes = 2,816 bytes of body, plus the 256-byte JV header.
- (c) The JV marks "present" and "nack" per header in `u128` bitsets. The pipeline's possibly-uncommitted ops plus the commit_max header (`pipeline + 1`) must each get a bit.

**Q4.**
- **Ignoring `log_view`** brings back the fork from `log_view_durable`. A replica that prepared many ops in an old view, which were later replaced and committed differently, can have the highest `op`. Its stale entries would overwrite committed entries from a newer view.
- **Truncating on a timeout** can drop a committed op. A commit only needs 3 copies; if those replicas are slow or partitioned, "couldn't fetch in 1 s" says nothing about whether the op committed. A client already has a reply for it, so the ledger forks from what clients saw.
- TigerBeetle truncates only with proof: a nack quorum (`replica_count − quorum_replication + 1` replicas that definitely didn't prepare it). Otherwise it waits (`awaiting_repair`) or declares the cluster stuck (`complete_invalid`). It never guesses.

**Q5.** Open-ended. Points a good answer weighs:
- **Deterministic primary:** simple, no split votes, clients compute the leader, and testing is easier. The cost: each dead designated primary costs one `view_change_status_timeout` (5 s in TigerBeetle) before moving on. With 2 of 6 dead and adjacent, worst case is about 2 extra timeouts.
- **Raft-style election:** picks a live, up-to-date replica directly. But it needs randomized timeouts and votes, and still needs log-transfer logic if you want PAR-style truncation.
- **Middle grounds:** skip known-dead replicas using recent ping data (TigerBeetle already skips a syncing replica, and the forfeit rule jumps to the view whose primary has the newest checkpoint). Or shorten the per-view timeout when the designated primary has been silent in pings.
- **Measure:** run the simulator with two crashed replicas and report the failover time distribution.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
