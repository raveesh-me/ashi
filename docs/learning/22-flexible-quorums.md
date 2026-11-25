# Concept 22: Flexible quorums

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, section "Flexible Quorums" (lines 523–538) and "Protocol Aware Recovery" (540–563)
- `../tigerbeetle/docs/internals/vsr.md`, section "Quorums" (lines 277–294)
- `../tigerbeetle/docs/operating/cluster.md`, "Fault Tolerance" and "Geographic Fault Tolerance"
- `../tigerbeetle/docs/concepts/safety.md`, "High Availability"
- `../tigerbeetle/src/vsr.zig`: `quorums()` (lines 1212–1273) and `test "quorums"` (1275–1300)
- `../tigerbeetle/src/constants.zig`: `replicas_max`, `standbys_max` (lines 24–28), `quorum_replication_max` doc comment (lines 121–131)
- `../tigerbeetle/src/config.zig`: `quorum_replication_max: u8 = 3` (line 157)
- `../tigerbeetle/src/vsr/replica.zig`: quorum fields (lines 241–252), `on_prepare_ok` (2241), `on_exit_view` (2546), `JVQuorum.quorum_headers` (11846), `standby()` (6946–6952)
- Paper (not in the clone): Howard, Malkhi, Spiegelman, *Flexible Paxos: Quorum intersection revisited*, 2016

## 1. The problem

Consensus (Concept 21) has two phases that both need "enough" replicas:

- **Replication:** a prepare is committed once enough replicas have it in their WAL. This happens on *every* batch, so its quorum size sets write latency.
- **View change:** a new primary gathers logs from enough replicas to learn every entry that might be committed. This happens rarely, only on failover.

Classic consensus uses the same size for both: a **majority**, `⌊N/2⌋ + 1`. For 5 replicas, the primary waits for 3 copies on every write. That is fine, but it treats a once-a-month event (failover) and a 100,000-times-a-second event (replication) the same.

## 2. The obvious solution, and why it fails

**Obvious fix 1: just commit with fewer acks.** Say 5 replicas, commit after 2 copies, and still use a majority of 3 for view changes.

```
Replicas:   A  B  C  D  E          A = primary
Op 9 commits with copies on {A, B}           (replication quorum 2)
A and B crash.
View change gathers logs from {C, D, E}      (majority 3)
→ none of them has op 9. The new primary drops it.
→ a client was told op 9 committed. It is gone.
```

The two quorums `{A, B}` and `{C, D, E}` **don't intersect**: `2 + 3 = 5`, which is not greater than 5.

**Obvious fix 2: always use majorities.** Safe, but with an even count it wastes a replica. Six replicas with majority 4 means waiting for 4 copies per write, and tolerating only 2 failures, the same as 5 replicas. That is why most systems run odd sizes (3, 5).

The real requirement is narrower than "majority everywhere". Each replication quorum only has to overlap each view-change quorum.

## 3. What TigerBeetle does

### 3.1 The Flexible Paxos rule

Flexible Paxos (Howard et al.) observed that Paxos needs only:

```
  |Q_replication| + |Q_view_change| > N
```

Any replication quorum must intersect any view-change quorum. Then the new primary is guaranteed to hear from at least one replica holding every possibly committed entry. Two replication quorums don't need to intersect each other.

> Note on naming: the Flexible Paxos paper calls the phase-1 (leader election, i.e. view change) quorum **Q1** and the phase-2 (replication) quorum **Q2**. Some summaries swap them. This lesson avoids the labels and uses TigerBeetle's names: `quorum_replication` and `quorum_view_change`.

The trade: make view change *more* expensive so replication can be *cheaper*. `constants.zig` puts it this way (lines 121–130):

```zig
/// The maximum number of nodes required to form a quorum for replication.
/// Majority quorums are only required across view change and replication phases (not within).
/// As per Flexible Paxos, provided `quorum_replication + quorum_view_change > replicas`:
/// 1. you may increase `quorum_view_change` above a majority, so that
/// 2. you can decrease `quorum_replication` below a majority, to optimize the common case.
/// This improves latency by reducing the number of nodes required for synchronous replication.
/// This reduces redundancy only in the short term, asynchronous replication will still continue.
/// The size of the replication quorum is limited to the minimum of this value and ⌈replicas/2⌉.
pub const quorum_replication_max = config.cluster.quorum_replication_max;
```

`quorum_replication_max = 3` (`src/config.zig:157`).

### 3.2 The quorum function

All quorum sizes come from one function, `quorums()` in `src/vsr.zig` (lines 1212–1273). The core:

```zig
// src/vsr.zig:1222-1247 (abridged)
// For replica_count=2, set quorum_replication=2 even though =1 would intersect.
// This improves durability of small clusters.
const quorum_replication = if (replica_count == 2) 2 else @min(
    constants.quorum_replication_max,
    stdx.div_ceil(replica_count, 2),
);

// For replica_count=2, set quorum_view_change=2 even though =1 would intersect.
// This avoids special cases for a single-replica view-change in Replica.
const quorum_view_change =
    if (replica_count == 2) 2 else replica_count - quorum_replication + 1;
// The view change quorum may be more expensive to make the replication quorum cheaper.
// The insight is that the replication phase is by far more common than the view change.
assert(quorum_view_change >= @divFloor(replica_count, 2) + 1);
assert(quorum_view_change + quorum_replication > replica_count);

// We need to have enough nacks to guarantee that `quorum_replication` was not reached,
// because if the replication quorum was reached, then it may have been committed.
const quorum_nack_prepare = replica_count - quorum_replication + 1;
assert(quorum_nack_prepare + quorum_replication > replica_count);
```

It also returns `majority = ⌈N/2⌉ + (1 if N even)` and `upgrade = N` (lines 1249–1264).

Reading the formulas:
- **Replication** = `min(3, ⌈N/2⌉)`, except 2 for `N = 2`. At most half the cluster, capped at 3 copies.
- **View change** = `N − replication + 1`, the smallest size that still intersects every replication quorum. The assert at line 1241 checks it is always at least a majority. That follows from replication ≤ ⌈N/2⌉, so it is a sanity check, not an extra rule.
- **Nack** = `N − replication + 1`. If that many replicas say "I never prepared op X", at most `replication − 1` can have it. So X can't have been committed, and it is safe to truncate (Concept 24).

### 3.3 The table, reproduced from the source

The unit test `test "quorums"` (`src/vsr.zig:1275-1300`) pins the values (it runs only when `quorum_replication_max == 3`, the default). For 1–6 replicas:

| Replica count `N` | 1 | 2 | 3 | 4 | 5 | 6 |
|---:|--:|--:|--:|--:|--:|--:|
| **Replication** | 1 | 2 | 2 | 2 | 3 | 3 |
| **View change** | 1 | 2 | 2 | 3 | 3 | 4 |
| **Nack** | 1 | **1** | 2 | 3 | 3 | 4 |
| Majority | 1 | 2 | 2 | 3 | 3 | 4 |
| Upgrade | 1 | 2 | 3 | 4 | 5 | 6 |
| Replication + view change | 2 | 4 | 4 | 5 | 6 | 7 |
| …is greater than `N`? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

The first three rows match `docs/internals/vsr.md` "Quorums" exactly (no discrepancy). The test arrays also cover `N = 7, 8` (replication 3, view change 5 and 6), but `replicas_max = 6` (`constants.zig:24`), so those can't be deployed.

Worked check for `N = 6`: replication = `min(3, ⌈6/2⌉) = 3`; view change = `6 − 3 + 1 = 4`; nack = `6 − 3 + 1 = 4`; `3 + 4 = 7 > 6` ✓.

The `N = 2` nack quorum of **1** is not a typo. Replication needs both replicas, so one replica that never prepared an op proves the op didn't commit. The test singles it out: "The nack quorum only differs from the view-change quorum when R=2" (`vsr.zig:1293`).

### 3.4 What each quorum gates, in the code

The replica stores the sizes at startup (`replica.zig:241-252`) and uses them here:

| Quorum | Gate | Code |
|---|---|---|
| `quorum_replication` | a prepare commits after this many `prepare_ok`s (any replicas, the primary's own included if it arrives) | `on_prepare_ok`, `replica.zig:2286-2304` |
| `quorum_view_change` | a replica changes view after this many `exit_view`s | `on_exit_view`, `replica.zig:2553-2556` |
| `quorum_view_change` | the new primary needs this many `join_view`s | `JVQuorum.quorum_headers`, `replica.zig:11877` |
| `quorum_nack_prepare` | an uncommitted op may be truncated in a view change | `replica.zig:11961` |

`quorum_majority` is computed and stored (`replica.zig:1155`, `1314`), but I found no other use of it in `replica.zig`.

### 3.5 Fault tolerance per cluster size

From the table: with the primary alive, the cluster commits while `N − replication` replicas are down. To elect a new primary, at most `N − view_change` may be down.

| `N` | Failures tolerated, primary alive | Failures tolerated incl. primary (view change) |
|--:|--:|--:|
| 3 | 1 | 1 |
| 4 | 2 | 1 |
| 5 | 2 | 2 |
| **6** | **3** | **2** |

Six replicas tolerate the same failover case as five (2), plus one more failure as long as the primary isn't among them. `docs/operating/cluster.md` says the same: available "provided that at least 3/6 machines have not failed (provided that the primary has not also failed) or provided that at least 4/6 machines have not failed (if the primary also failed and a new primary needs to be elected)."

### 3.6 Verifying the availability math in ARCHITECTURE.md

ARCHITECTURE.md (lines 534–538):

> If in a cluster of `6` three replicas crash simultaneously, there's a 50% chance that the cluster remains available. If the replicas are crashing one by one the chance is higher. When only `4` replicas remain, the chance that the next one to crash would be a primary is only 25%.

**Claim 1: three at once → 50%.** With 3 survivors the cluster can still replicate (3 = replication quorum) but can't change view (needs 4). So it survives exactly when the primary is not among the 3 that crashed. If the crashes hit uniformly random replicas:

```
P(primary survives) = C(5,3) / C(6,3) = 10 / 20 = 50%      (equivalently 3/6)
```
✓

**Claim 2: one by one → higher.** Crash 1 (6 → 5) and crash 2 (5 → 4): even if the primary dies, 5 or 4 survivors still form a view-change quorum of 4, so a new primary is elected. Crash 3 (4 → 3): fatal only if it hits the current primary, which is 1 of 4.

```
P(available after 3 sequential crashes) = 1 − 1/4 = 75%
```
✓ (25% matches the doc's "only 25%".) This assumes each view change finishes before the next crash.

**Claim 3: the three-site layout.** `docs/concepts/safety.md` recommends six replicas across three providers, two each, which "is guaranteed to tolerate a complete outage of any cloud provider and will likely survive even if one extra replica fails". Losing a site leaves 4 = view-change quorum ✓. One more random failure among the 4 is fatal only if it is the primary: 75% survival, "likely" ✓.

Caveat on all three: "available" here means quorum math only. It assumes the survivors are reachable from each other and from clients, and are not stuck state-syncing.

### 3.7 Standbys

A **standby** is a node with index `≥ replica_count`; up to `standbys_max = 6` (`constants.zig:26`), so `members_max = 12`. Standbys:
- receive every prepare and commit message (the primary sends "to every other replica and standby", Concept 23);
- **never send `prepare_ok`** (`send_prepare_ok` returns early, `replica.zig:8702`);
- ignore `exit_view` and `join_view` ("misdirected message (standby)", `replica.zig:6792`, `6869`), and never become primary.

So standbys appear in no quorum. Adding standbys never changes the table above, and a standby failure never affects availability. They are extra warm copies. Promoting one into an active replica would need reconfiguration, which `vsr.md` lists as "TODO (Unimplemented)". The CLI marks standbys "Experimental: standbys don't have a concrete practical use-case yet" (`src/tigerbeetle/cli.zig:42`).

### 3.8 Why 6 replicas are recommended

`docs/operating/cluster.md`: "The optimal, recommended size for any production cluster is 6 replicas." The reasons, drawn together:

1. **Same write latency as 5.** Both wait for 3 copies, which means 2 remote acks. But with 6 the primary picks the 2 fastest of 5 backups instead of 4. The slowest replica is masked more often (Concept 23).
2. **One more failure tolerated in normal operation** (3 vs 2, see 3.5).
3. **Site loss:** 3 sites × 2 replicas survive a full site outage, plus probably one more failure (3.6). With 5 replicas over 3 sites (2+2+1), losing a 2-replica site leaves 3, which is enough to elect (view change = 3 for N=5). But one more failure leaves 2, below the replication quorum of 3.
4. **Durability floor of 3.** Every committed prepare is in at least 3 WALs, thanks to the `quorum_replication_max = 3` cap. That holds for 5 and 6 alike.
5. **It is the maximum.** `replicas_max = 6`. The cap on replication quorum also keeps view change at `N − 2`: for N=6 that is 4, still a reasonable number of survivors.

Why not cap replication lower, at 2 for 6 replicas? The rule would allow it (view change would then be 5). But a commit would then rest on 2 disks, and failover would need 5 of 6 alive. The cap of 3 trades a little latency for durability and failover tolerance.

## 4. What it means for ashi

**Compute quorums in one pure `const fn`** and pin them with a table test, as TigerBeetle does:

```rust
pub struct Quorums { pub replication: u8, pub view_change: u8, pub nack: u8 }

pub const fn quorums(n: u8) -> Quorums {
    assert!(n >= 1 && n <= REPLICAS_MAX);
    let replication = if n == 2 { 2 } else { min_u8(QUORUM_REPLICATION_MAX, n.div_ceil(2)) };
    let view_change = if n == 2 { 2 } else { n - replication + 1 };
    let nack = n - replication + 1;
    assert!(replication + view_change > n);
    assert!(nack + replication > n);
    Quorums { replication, view_change, nack }
}
const _: () = { let q = quorums(6); assert!(q.replication == 3 && q.view_change == 4); };
```

**Rust features that help:**
- `const fn` + `const _: () = assert!(…)` gives compile-time proofs like Zig's `comptime`.
- A fixed-width bitset (`u8` or `u16` for ≤ 12 members) for "who acked" is `Copy`, allocation-free, and makes `count_ones()` the quorum check.

**Patterns:**
- Count each sender once. TigerBeetle's `count_message_and_receive_quorum_exactly_once` uses a per-prepare bitset. A duplicated `prepare_ok` (Concept 28) must not count twice.
- Act **exactly** when the count reaches the quorum, not on every message after. This avoids double-commit logic.
- Keep standbys out of every bitset that feeds a quorum, by construction (separate index range), not by an `if`.

**Crates:** none needed. This is 30 lines of arithmetic. TigerBeetle has zero dependencies (Concept 13).

**Pitfalls specific to Rust:**
- `n / 2 + 1` vs `n.div_ceil(2)`: off-by-one on even `n`. Test every `n` in `1..=REPLICAS_MAX`.
- `u8` subtraction `n - replication` panics in debug and wraps in release. Assert the precondition, or use `checked_sub`.
- Don't make quorum sizes runtime-configurable per replica. They must be identical cluster-wide, so derive them from the cluster config fingerprint (Concept 20).

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt**. Flexible quorums are pure arithmetic with a proven safety rule, and they directly buy latency and site-loss tolerance.

## 5. Drill questions

### Q1. Recall

Write the replication, view-change and nack quorums for 3, 4, 5 and 6 replicas from memory. Then derive each from the formulas in `quorums()`.

> _Your answer:_

### Q2. Mechanism

Explain why the nack quorum is `N − quorum_replication + 1`, not a majority. Use `N = 6`: what exactly does 4 nacks prove, and why would 3 nacks not be enough?

> _Your answer:_

### Q3. Back-of-envelope

A 6-replica cluster runs 2 replicas in each of 3 sites. Site A (which holds the primary) goes down. (a) Can the cluster elect a new primary? (b) While still degraded, one more replica fails at random. What is the probability the cluster stays available? (c) Repeat (b) for 5 replicas laid out 2+2+1, losing a 2-replica site.

> _Your answer:_

### Q4. What breaks if…

…an operator sets `quorum_replication_max = 2` on a 6-replica cluster, but a bug keeps the view-change quorum at 4? Construct a concrete history that loses a committed transfer.

> _Your answer:_

### Q5. ashi design (open-ended)

Should ashi let operators choose the replication quorum (e.g. 2, 3 or 4 of 6) per cluster? Weigh latency, durability, failover tolerance and the risk of misconfiguration. Where would the setting live, and how would you stop it from changing over the cluster's lifetime?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- N=3: 2 / 2 / 2. N=4: 2 / 3 / 3. N=5: 3 / 3 / 3. N=6: 3 / 4 / 4.
- Derivation: replication = `min(3, ⌈N/2⌉)` → 2, 2, 3, 3. View change = `N − replication + 1` → 2, 3, 3, 4. Nack = `N − replication + 1` → same as view change for N ≥ 3.

**Q2.**
- An op could have committed only if `quorum_replication` replicas prepared it.
- Truncation is safe if we *prove* that didn't happen, i.e. that at most `quorum_replication − 1` replicas could have it.
- If `k` replicas nack, at most `N − k` hold it. We need `N − k ≤ quorum_replication − 1`, so `k ≥ N − quorum_replication + 1`.
- For N=6: 4 nacks leave at most 2 possible holders < 3, so it can't have committed. With 3 nacks, 3 replicas might hold it, which is a full replication quorum, so it might be committed and must not be dropped.
- A majority (4 for N=6) happens to coincide here, but for N=5 the nack quorum is 3 because replication is 3. The formula, not "majority", is what matters.

**Q3.**
- (a) Yes. 4 survivors = view-change quorum 4. A new primary is elected (after a timeout per dead designated primary).
- (b) 4 replicas, one of them primary. A random failure hits the primary with probability 1/4, which leaves 3 survivors that can't elect. Otherwise 3 survivors including the primary still meet replication quorum 3. Available with probability **3/4 = 75%**.
- (c) 5 replicas, lose 2 → 3 left. View change for N=5 is 3, so election succeeds. One more failure → 2 left, below replication quorum 3, so unavailable whichever replica fails: **0%**. Hence "6 across 3 sites".

**Q4.**
- With replication 2 and view change 4: `2 + 4 = 6`, not `> 6`. The quorums can miss each other.
- History: primary A prepares op 50. A and B write it and A commits (2 acks) and replies "ok" to the client. A and B then crash before C–F receive op 50 (lost or delayed prepares).
- C, D, E, F form a view-change quorum of 4. None has op 50. Under TigerBeetle's own rule they would even produce 4 nacks, so op 50 is truncated. The client's committed transfer is gone.
- This is why the code asserts `quorum_view_change + quorum_replication > replica_count` at startup; a correct build derives view change = 5.

**Q5.** Open-ended. Points a good answer weighs:
- **Latency:** fewer acks means a lower tail. With 2 of 6 you wait for 1 remote ack.
- **Durability:** a commit lives on only that many disks until async replication catches up. Is 2 enough under Concept 14's fault model?
- **Failover:** view change = `N − replication + 1`. Replication 2 means 5 of 6 needed to elect, a big availability loss. Replication 4 means elect with 3 but wait for 3 remote acks.
- **Misconfiguration:** quorums must be identical on every replica and must never change while data exists. A value that differs across replicas, or changes on restart, silently breaks intersection.
- **Placement:** make it part of the cluster config that is written into the data file at format time and fingerprinted (like `ConfigCluster` in Concept 20). Reject mismatched peers. Changing it would need a reconfiguration protocol. A reasonable answer is to fix it at 3 as TigerBeetle does, and revisit only with simulator evidence.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
