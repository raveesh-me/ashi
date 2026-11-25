# Concept 26: Cluster clock (Marzullo's algorithm)

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Time" and "Determinism"
- `../tigerbeetle/docs/coding/time.md`, all sections
- `../tigerbeetle/docs/concepts/safety.md`, section "High Availability" (leader-based timestamping, no leases)
- `../tigerbeetle/docs/reference/transfer.md`, section "`timeout`"
- `../tigerbeetle/docs/internals/vsr.md`, section "Protocol: Ping (Replica-Replica)"
- `../tigerbeetle/src/vsr/clock.zig`: module doc (1–79), `Sample`/`Epoch` (100–161), `init` (187–231), `learn` (250–326), `realtime` (337), `realtime_synchronized` (346–362), `tick` (382–394), `estimate_asymmetric_delay` (418–443), `synchronize` (445–512), `after_synchronization` (514–574), `window_tuples` (576–599), `minimum_one_way_delay` (601–607)
- `../tigerbeetle/src/vsr/marzullo.zig`: `Marzullo` (8–132), tests (134–200)
- `../tigerbeetle/src/vsr/replica.zig`: `Clock.init` with quorum (1190–1211), `ping_timeout` (1354–1358), `on_ping` (1842), `on_pong` (1891–1909), `on_request` clock check (1966–1985), `on_primary_abdicate_timeout` (3699–3710), `primary_pipeline_prepare` timestamp (7354–7365, 7409–7428), view-change `timestamp_max` (9753–9761)
- `../tigerbeetle/src/vsr/message_header.zig`: `Ping` (365–415), `Pong` (421–455), `Prepare.timestamp` (733–735)
- `../tigerbeetle/src/state_machine.zig`: `prepare` (1146–1177), `prepare_delta_nanoseconds` (1182–1212), per-event timestamp (3005)
- `../tigerbeetle/src/constants.zig` (720–775), `../tigerbeetle/src/config.zig` (120–129)

## 1. The problem

A financial ledger needs **wall-clock time** inside its business logic:
- **Two-phase transfer timeouts.** A pending transfer reserves funds. If it isn't posted or voided within `timeout` seconds, it expires and releases them (`time.md`, "Physical Time"; `transfer.md`, "`timeout`").
- **Audit.** Regulators want to know *when* a transfer happened.
- **Ordering and keys.** Every object gets a unique `u64` nanosecond timestamp, which also serves as its synthetic primary key in the LSM (Concept 19).

But the state machine is replicated (Concept 20). Every replica must reach the *same* result from the *same* log. If each replica read its own OS clock while executing, replica A might expire a transfer that replica B keeps open. They would diverge.

And OS clocks are bad:
- They differ between machines.
- They jump backwards when NTP (Network Time Protocol) corrects them.
- They drift without limit if a machine loses contact with its NTP servers.
- An operator can set one wrong by hours.

## 2. The obvious solution, and why it fails

**"Each replica reads `clock_gettime(CLOCK_REALTIME)` during execution."** This breaks determinism immediately: same log, different results.

**"Let the client send the timestamp."** Clients' clocks are even less trustworthy. A malicious or broken client could backdate transfers or dodge timeouts. `clock.zig:7–9`: time "can't be supplied by the client, as its clock can be unreliable."

**"The primary stamps each batch with its own OS time."** This is deterministic, since the timestamp travels in the log. But it fails the other two ways:
- NTP can step the primary's clock backwards, giving a later transfer a smaller timestamp. That breaks the ordering and the LSM key order.
- A primary cut off from NTP, or set wrong by an operator, drifts with no check. `clock.zig:41–43`: "an operator error which incorrectly adjusts primary's local clock to be far in the future, which, due to monotonicity requirement, could render the cluster completely unusable." Every later timestamp must exceed the bogus one, so you would be stuck in the future.

**"Use leader leases or TrueTime-style bounds."** This needs tight hardware clock guarantees. TigerBeetle explicitly "does not depend on synchronized system clocks, does not use leader leases" (`safety.md`).

## 3. What TigerBeetle does

The module doc states it plainly: "we don't use time to drive consensus, we use consensus to drive time!" (`clock.zig:4–5`).

### 3.1 Layer 1: only the primary stamps, and the stamp is in the log

When the primary turns a request into a prepare, it assigns a timestamp. Replicas never read the clock during execution; they read `prepare.header.timestamp`. From `ARCHITECTURE.md`, "Time": "The state machine can't access OS to get time directly as that would violate determinism. Instead, the TigerBeetle primary injects a specific timestamp into the logic of the state machine when it converts a request to a prepare."

The code (`replica.zig:7354–7365`):

```zig
// Guard against the wall clock going backwards by taking the max with timestamps
// issued:
self.state_machine.prepare_timestamp = @max(
    // The cluster `commit_timestamp` may be ahead of our `prepare_timestamp` because
    // this may be our first prepare as a recently elected primary:
    @max(
        self.state_machine.prepare_timestamp,
        self.state_machine.commit_timestamp,
    ) + 1,
    @as(u64, @intCast(request.realtime)),
);
assert(self.state_machine.prepare_timestamp > self.state_machine.commit_timestamp);
```

Then `state_machine.prepare` adds one nanosecond per event in the batch (`state_machine.zig:1146–1212`). For `create_transfers` this is `@divExact(batch.len, @sizeOf(Transfer))`. The prepare header's `timestamp` is "the batch's highest timestamp" (`message_header.zig:733–735`). At commit, event *i* of *N* gets `timestamp - N + i + 1` (`state_machine.zig:3005`).

```
 request arrives at primary, cluster time = T
 prepare_timestamp = max(prev + 1, T)          say = 1_000_000
 batch of 3 transfers → prepare_timestamp += 3 → header.timestamp = 1_000_003
 transfers get        1_000_001, 1_000_002, 1_000_003
```

Two properties fall out:
- **Strict monotonicity.** Each timestamp is at least the previous one + 1, even if the OS clock goes backwards. "TigerBeetle also guarantees that the time observed by the state machine is strictly monotonic" (`ARCHITECTURE.md`).
- **It survives restarts and view changes.** A replica relearns the latest timestamp by replaying the WAL (`clock.zig:34–37`). A new primary raises `prepare_timestamp` to the highest timestamp in the view-change quorum before it prepares anything (`replica.zig:9753–9761`).

### 3.2 Layer 2: NTP is the ultimate source

`ARCHITECTURE.md`: "The ultimate source of time for the TigerBeetle cluster is Network Time Protocol (NTP)." TigerBeetle doesn't run its own time servers. Each replica's OS keeps its clock disciplined by NTP. TigerBeetle's job is to **detect and bound** the case where a primary's clock has wandered.

### 3.3 Layer 3: cluster time from ping/pong

Replicas ping each other every second: `ping_timeout.after = 1_000 / constants.tick_ms` ticks, with `tick_ms = 10` (`replica.zig:1354–1358`, `config.zig:120`). Each round trip measures one remote clock:

```
   replica A                                       replica B
   m0 = A.monotonic()
   ping{ping_timestamp_monotonic = m0} ───────────►
                                                   t1 = B.realtime()
                    ◄──────────── pong{ping_timestamp_monotonic = m0,
                                       pong_timestamp_wall = t1}
   m2 = A.monotonic()
   clock.learn(B, m0, t1, m2)
```

`learn` (`clock.zig:250–326`) computes:

```
round_trip_time = m2 - m0
one_way_delay   = round_trip_time / 2
t2              = window.realtime + (m2 - window.monotonic)   // A's wall time "now"
clock_offset    = t1 + one_way_delay - t2                      // B's clock minus A's
```

Design choices worth noticing:
- **Monotonic clocks for intervals, wall clock once per window.** `t2` is derived from the wall-clock reading taken at the window's start plus elapsed *monotonic* time. This guards "against any jumps in the system's realtime clock from impacting our measurements" (`clock.zig:120–124`).
- **The best sample wins.** For each source, keep only the sample with the smallest one-way delay (`minimum_one_way_delay`, `clock.zig:601`). "The less network delay, the more likely we have an accurate clock offset measurement."
- **Asymmetric delay correction.** One ping or pong may be stuck behind a large prepare in a send queue. `estimate_asymmetric_delay` (`clock.zig:418–443`) compares the sample with the previous epoch's best for that source. If the new RTT is larger *and* the offset moved by more than 10 ms, it assumes the extra delay was all on one leg and corrects for it. The comment admits a limit: *persistent* asymmetry is undetectable.
- **Pings carry the on-disk view**, "to avoid disrupting clock synchronization while the view is being updated" (`message_header.zig:374–375`).
- Standbys are tracked but **never influence** active replicas' clocks (`replica.zig:1190–1211`; `on_pong` ignores standbys).

### 3.4 Marzullo's algorithm

Each sample becomes an **interval** for the true offset of the cluster clock relative to ours:

```
[ clock_offset - (one_way_delay + tolerance),
  clock_offset + (one_way_delay + tolerance) ]          (window_tuples, clock.zig:576–599)
```

Our own clock is a source too, with offset 0 and delay 0 (`Epoch.reset`, `clock.zig:143–147`).

Marzullo's algorithm (Keith Marzullo, 1984 PhD thesis; `marzullo.zig:4–7`) finds "the smallest interval consistent with the largest number of sources". It is a sweep line:

```zig
// marzullo.zig:57–97, simplified
sort tuples by offset (lower before upper on ties, then by source index)
for each tuple:
    count += (lower ? 1 : -1)
    if count > best: best = count; interval = [this.offset, next.offset]
    elif count == best and next is upper and narrower: take the narrower interval
sources_true  = best
sources_false = sources - best
```

`sources_true` are "truechimers" and `sources_false` are "falsetickers" (NTP terminology). NTP uses a looser variant (the intersection algorithm) that returns a *larger* interval for more statistics; TigerBeetle wants the *smallest* (`marzullo.zig:5–7`).

The sort deserves a note. It uses insertion sort "for safety", and the comparator breaks every tie by source index "so that different sort algorithms sort the same way" (`marzullo.zig:55, 127–130`). That is determinism thinking, applied even to the clock.

### 3.5 Finding the tightest agreement: the tolerance loop

Clocks don't advertise their error. So `synchronize` (`clock.zig:445–512`) flips the problem: start with the loosest tolerance and **halve it** while a quorum still agrees.

```zig
var tolerance: u64 = clock_offset_tolerance_max;   // 10 s
while (!terminate and rounds < 64) : (tolerance /= 2) {
    if (tolerance == 0) terminate = true;
    rounds += 1;
    const interval = Marzullo.smallest_interval(self.window_tuples(tolerance));
    if (interval.sources_true < self.quorum) break;
    self.window.synchronized = interval;   // keep the tightest interval that still has a quorum
}
```

`self.quorum` is the **replication quorum** (`replica.zig:1205`): 2 of 3, or 3 of 6. `ARCHITECTURE.md`: "the cluster aggregates timing information from a replication quorum of replicas."

### 3.6 Worked example

Three replicas, so the quorum is 2. We are replica 0. Best samples this window:

| Source | clock_offset | one_way_delay | note |
|---|---:|---:|---|
| R0 (self) | 0 ms | 0 ms | |
| R1 | +30 ms | 5 ms | healthy, slightly ahead |
| R2 | +2,000 ms | 5 ms | lost NTP, 2 s fast |

Each interval is `offset ± (delay + T)`:

| T (ms) | R0 | R1 | R2 | Best overlap | sources_true | Keep? |
|---:|---|---|---|---|---:|---|
| 10,000 | [−10000, 10000] | [−9975, 10035] | [−8005, 12005] | [−8005, 10000] | 3 | ✓ |
| 2,500 | [−2500, 2500] | [−2475, 2535] | [−505, 4505] | [−505, 2500] | 3 | ✓ |
| 1,250 | [−1250, 1250] | [−1225, 1285] | [745, 3255] | [745, 1250] | 3 | ✓ |
| 625 | [−625, 625] | [−600, 660] | [1370, 2630] | [−600, 625] | **2** (R2 is a falseticker) | ✓ |
| 39.06 | [−39.06, 39.06] | [−14.06, 74.06] | … | [−14.06, 39.06] | 2 | ✓ |
| 19.53 | [−19.53, 19.53] | [5.47, 54.53] | … | **[5.47, 19.53]** | 2 | ✓ |
| 9.77 | [−9.77, 9.77] | [15.23, 44.77] | … | no overlap | 1 | ✗ stop |

(Rows for 5,000, 312.5, 156.25 and 78.125 ms are omitted; they also keep a quorum.)

The sweep for the T = 625 row:

```
offset:  -625(L,R0)  -600(L,R1)   625(U,R0)   660(U,R1)   1370(L,R2)   2630(U,R2)
count:       1           2            1           0            1            0
best:        1 → [-625,-600]  2 → [-600, 625]                    (1 is not > 2)
```

Result: cluster offset ∈ **[+5.47 ms, +19.53 ms]** relative to R0's clock. R2 is out-voted.

`realtime_synchronized` (`clock.zig:346–362`) then **clamps** the OS clock into that window:

```
cluster_time = clamp(system_now,
                     epoch.realtime + elapsed + lower_bound,
                     epoch.realtime + elapsed + upper_bound)
```

So R0's timestamps get nudged about 5.5 ms forward, toward agreement with R1. If R2 were primary, its +2 s clock would be clamped back into the cluster's interval. Its bad clock can't push the cluster into the future. `after_synchronization` logs a warning when the system clock is ≥ 50 ms outside the interval (`clock.zig:533–538`).

### 3.7 Windows, epochs and the tolerance constants

- **Window:** samples being collected. It must be at least `clock_synchronization_window_min = 2 s` old before trying to synchronize. It expires at `clock_synchronization_window_max = 20 s` without agreement, discarding stale samples ("This eliminates the impact of gradual clock drift", `constants.zig:768–774`).
- **Epoch:** the last synchronized window, used for reads. It expires after `clock_epoch_max = 60 s` if no new window synchronizes: "Gradual clock drift prevents us from using an epoch for more than a few seconds" (`clock.zig:382–394`).
- **`clock_offset_tolerance_max = 10 s`** (`config.zig:126`): the starting tolerance and the hard limit. From `constants.zig:750–755`: "The cluster will be unavailable if the majority of clocks are all further than this value apart. Decreasing this reduces the probability of reaching agreement on synchronized time. Increasing this reduces the accuracy of synchronized time."

### 3.8 When the clock can't synchronize

1. `synchronize` logs "synchronization failed, partitioned" (half or fewer sources sampled) or "no agreement" (`clock.zig:449–466`).
2. After 60 s, `tick` logs "no agreement on cluster time (partitioned or too many clock faults)" and resets the epoch. `realtime_synchronized` then returns `null`.
3. The primary's `on_request` gets `null`. It **drops the request**, logs "dropping (clock not synchronized)", and starts `primary_abdicate_timeout` (`replica.zig:1971–1983`).
4. When that fires, `primary_abdicating = true`, which "pauses outgoing commit heartbeats" (`replica.zig:529–531, 3699–3710`). Backups see no heartbeats and start a view change, hopefully toward a primary whose clock agrees with a quorum.
5. Clients retry forever (Concept 27), so no request is lost. They just wait.

Availability is traded for correct time. A ledger would rather pause than stamp transfers with a wrong time.

Special case: a one-replica cluster "cannot synchronize". `synchronization_disabled = true` and the raw OS clock is used (`clock.zig:225–226, 346–348`).

> **Docs vs code:** consistent. `clock.zig`'s module doc says the state machine "assigns `prepare_timestamp + object_index`". In code the header carries the batch's *highest* timestamp and objects get `timestamp - N + index + 1` (`state_machine.zig:3005`). Same numbers, counted from the other end.

## 4. What it means for ashi

**Rust features that help or hurt:**
- `std::time::Instant` is monotonic and `SystemTime` is wall-clock, the same split TigerBeetle makes. But both are *global*. For determinism and simulation, hide them behind a trait (`trait Time { fn monotonic(&self) -> u64; fn realtime(&self) -> i64; }`) and pass it in, as TigerBeetle passes `Time` to `Clock.init`. A `SimTime` with programmable drift and offset lets tests reproduce clock faults; see `TimeSim` used in `clock.zig`'s tests.
- Use integer nanoseconds (`u64`/`i64`) everywhere. Never use `f64` seconds; floating point is a determinism hazard.
- `Duration` is unsigned, but clock offsets are signed. Use `i64` for offsets and watch `as` casts. TigerBeetle uses `@intCast`, which panics on overflow in safe builds. In Rust use `i64::try_from(..).unwrap()` or `checked_*`, not `as`, which wraps silently.

**Patterns to copy:**
- The state machine never reads a clock. The timestamp is an input carried in the prepare.
- `prepare_timestamp = max(prev + 1, cluster_time)`, raised at view change to the quorum's max.
- Marzullo with a **total** comparator (tie-break on source id) and a fixed-size tuple buffer (`2 × replica_count`, preallocated).
- Clamp, don't replace: use the OS clock when it agrees, and pull it into the agreed interval when it doesn't.
- No quorum agreement → the primary stops accepting work and abdicates.

**Crates:** nothing needed. Marzullo is about 60 lines. NTP client crates (`sntpc`, etc.) solve a different problem; leave NTP to the OS (chrony/ntpd), as TigerBeetle does.

**Pitfalls:**
- `tokio::time::interval` for pings brings in a runtime timer you can't control in simulation. Drive pings from your own tick, as TigerBeetle uses `tick_ms = 10`.
- Don't let the asymmetric-delay correction or the "minimum one-way delay" choice depend on `HashMap` iteration. Index sources by replica number in a plain array.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt.** Primary-assigned, strictly monotonic, quorum-clamped timestamps are cheap, small, and remove a whole class of ledger bugs. The only open question is whether ashi's tolerance and window constants should differ.

## 5. Drill questions

### Q1. Recall

What are the default values of `clock_offset_tolerance_max`, `clock_epoch_max`, `clock_synchronization_window_min` and `clock_synchronization_window_max`? Which quorum must agree for the clock to synchronize?

> _Your answer:_

### Q2. Mechanism

`learn` computes A's current wall time as `window.realtime + (m2 - window.monotonic)` instead of calling `realtime()` again. Why? What failure does this protect against?

> _Your answer:_

### Q3. Back-of-envelope / worked example

Five replicas (quorum 3). You are R0. Best samples: R1 offset +10 ms, delay 2 ms; R2 offset −8 ms, delay 2 ms; R3 offset +500 ms, delay 2 ms; R4 not sampled. Starting at T = 10 s and halving, find the last tolerance at which ≥ 3 sources agree, and the resulting interval. Which source is the falseticker?

> _Your answer:_

### Q4. What breaks if…

…the primary used `realtime_synchronized()` directly as the prepare timestamp, without the `max(prev + 1, …)` guard? Give a concrete sequence of events.

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle makes the cluster unavailable rather than accept a primary whose clock disagrees with a quorum. For ashi, would you ever accept a "degraded time" mode, e.g. keep processing transfers *without* timeouts while the clock is unsynchronized? What would that do to auditability and to the timestamp-as-key invariant (Concept 19)?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** `clock_offset_tolerance_max = 10 s`, `clock_epoch_max = 60 s`, `clock_synchronization_window_min = 2 s`, `clock_synchronization_window_max = 20 s` (`config.zig:126–129`). The clock's `quorum` is the replication quorum (`replica.zig:1205`), counting the replica's own clock as one source: 2 of 3, 3 of 5 or 6.

**Q2.** The OS wall clock can jump at any moment (an NTP step, an operator change). If A read `realtime()` at `m2`, a jump between pings would show up as a fake clock offset for B. Taking one wall-clock reading at the window's start and adding *monotonic* elapsed time keeps all samples in one window on a consistent baseline. The comment says it guards "against any jumps in the system's realtime clock from impacting our measurements".

**Q3.** Intervals are `offset ± (2 + T)` for R1–R3 and `0 ± T` for R0. R1 = [8 − T, 12 + T] and R2 = [−10 − T, −6 + T]. They overlap only if 8 − T ≤ −6 + T, so T ≥ 7. When they do, the overlap [8 − T, −6 + T] sits inside R0's [−T, T], so all three agree exactly when T ≥ 7. Halving from 10,000 ms: 10000, 5000, 2500, 1250, 625, 312.5, 156.25, 78.125, 39.06, 19.53, **9.77**, 4.88. At T = 9.77: R0 [−9.77, 9.77], R1 [−1.77, 21.77], R2 [−19.77, 3.77]; the overlap of all three is **[−1.77, 3.77]**, with sources_true = 3. (R3 [488.2, 511.8] is far away; it dropped out of the 4-way overlap once T fell below about 250 ms.) At T = 4.88: R1 [3.12, …] and R2 […, −1.12] no longer overlap, so only 2 agree and the loop stops. Answer: interval ≈ [−1.8 ms, +3.8 ms]. **R3** is the falseticker.

**Q4.** Example: the primary stamps batch A at cluster time 1,000,000,000. Then NTP steps the primary's OS clock back 50 ms, and the clamped value can also move back within the interval as the epoch is replaced. Batch B is stamped 950,000,000. Now B's transfers have *smaller* timestamps than A's even though they committed later. That breaks the strict ordering promised in `time.md` and the LSM's timestamp-key order (Concept 19), and it can make a pending transfer's expiry fire before the transfer existed. The `max(prev + 1, …)` (and the `commit_timestamp` term, for a new primary) prevents all of that.

**Q5.** Open-ended. A degraded mode would issue timestamps no quorum vouches for. Audit timestamps lose their meaning. Pending-transfer expiry becomes either impossible or wrong. Worse, if the bad clock is *ahead*, monotonicity forces every later timestamp to stay ahead, which is the "stuck in the future" failure from `clock.zig:41–43`. A defensible middle ground is none for writes. Reads with no timestamp (e.g. lookups) could arguably proceed, but in TigerBeetle reads also go through consensus, so that changes the consistency model too (Concept 33).

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
