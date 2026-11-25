# Concept 28: Assuming the network can drop, duplicate or reorder messages

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Message Passing", "Star Replication" (the 8 fallacies link) and "Hash-Chaining"
- `../tigerbeetle/docs/internals/vsr.md`, sections "Commands", "Protocol: Repair Journal" (gaps from nonconsecutive ops)
- `../tigerbeetle/src/vsr/message_header.zig`: `Header` (14–76), `calculate_checksum`/`calculate_checksum_body` (108–124), `invalid` (167–200)
- `../tigerbeetle/src/vsr/checksum.zig`: module doc (1–30)
- `../tigerbeetle/src/message_buffer.zig`: doc (12–20), `InvalidReason` (45–52), `advance_header` (115–161), `advance_body` (163–186)
- `../tigerbeetle/src/message_pool.zig`: `messages_max_client` (34–48), `messages_max_replica` (53–101), `Message` (107–180), `init_capacity` (194–223), `get_message` (254–271), `unref_base` (288–300), `CommandMessageType` (303–353)
- `../tigerbeetle/src/message_bus.zig`: struct (21–96), `init_tcp` (269–284), `tick_connect` (337–350), `reclaim_connection` (459–513), `recv_callback` (676), `recv_buffer_consume` (880–905), `send_message_to_replica` (909–922), `send_message_to_client` (924–940), `send_message` (942–971), `send_callback` (1022–1058), `terminate` (1066)
- `../tigerbeetle/src/message_bus_fuzz.zig`: header (1–20), `send_corrupt_probability` (94, 444, 661–667)
- `../tigerbeetle/src/testing/packet_simulator.zig`: `PacketSimulatorOptions` (13–50), `PartitionMode` (60–78), `Link` (99–124), `should_drop`/`should_replay`/clogging (271–297), `step` (366), `tick` (387), `submit_packet` (425–458), `submit_packet_finish` (460–501), `packet_delay_default` (528–533)
- `../tigerbeetle/src/testing/cluster/network.zig`: `send_message` (230–262), `packet_deliver` (297–340)
- `../tigerbeetle/src/vopr.zig`: network options (444–464)
- `../tigerbeetle/src/vsr/replica.zig`: `QuorumCounter` (141), timeouts (1354–1449), `on_prepare_timeout` (3629–3698), `count_message_and_receive_quorum_exactly_once` (4143), request→prepare buffer reuse (7393)
- `../tigerbeetle/src/constants.zig` (416–484, 747), `../tigerbeetle/src/config.zig` (104–114)
- The 8 fallacies: linked from `ARCHITECTURE.md` (Wikipedia, "Fallacies of distributed computing"); the list itself is standard and not in the clone

## 1. The problem

A consensus protocol is a conversation between machines. The network carrying it can:
- **drop** a message (a full buffer, a reset connection, a flaky NIC);
- **duplicate** it (a retry after a timeout whose original was only delayed);
- **reorder** it (two paths, or a reconnect, or a retry overtaking the original);
- **delay** it without limit (congestion, a paused VM, a clogged link);
- **corrupt** it (bad memory, a NIC bug, a middlebox);
- **partition**: some nodes can talk and others can't, sometimes in one direction only.

For a ledger the stakes are concrete. A lost `prepare_ok` must not lose a commit. A duplicated `request` must not debit twice. A reordered `prepare` must not land in the wrong slot. A corrupted `reply` must not tell a client "success" for the wrong transfer.

## 2. The obvious solution, and why it fails

**"We use TCP. TCP is reliable and ordered."** TCP gives reliable, ordered delivery only *within one connection, while it stays up*. In practice:
- **Connections break.** Bytes still in the kernel's send buffer at a reset are lost. After a reconnect you can't tell which messages arrived.
- **There is more than one path.** A TigerBeetle client sends each request to the primary *and* a random backup (`client.zig:736–750`). Messages between two replicas can arrive on a fresh connection while older ones are still in flight on the old one.
- **Retries create duplicates.** The app layer must retry after timeouts (Concept 27), so duplicates are guaranteed, not exceptional.
- **TCP's checksum is weak.** It's a 16-bit ones'-complement sum, which some multi-bit errors slip past. At billions of messages, "rare" happens.
- **Bounded queues must drop.** With static allocation (Concept 11), a send queue can't grow. When it is full, something gets dropped.

So a protocol that *assumes* exactly-once, in-order delivery is correct only while the transport is perfect. Every transport bug then becomes a consensus bug. The transport also can't be tested in a deterministic simulator without simulating TCP itself.

This is the first of the **8 fallacies of distributed computing**, which `ARCHITECTURE.md` cites under "Star Replication": "the network is reliable".

## 3. What TigerBeetle does

### 3.1 The contract: assume almost nothing

From `ARCHITECTURE.md`, "Message Passing":

> TigerBeetle is agnostic of the underlying transport protocol and requires only a very weak message passing semantics. TigerBeetle assumes that messages might be dropped, duplicated, reordered, and corrupted (in non-byzantine way). Although specific `MessageBus` is not tested in VOPR, bugs in `MessageBus` are unlikely to affect correctness, because the contract for the transport layer is intentionally very weak.

**Non-byzantine** means faults are accidents, not attacks. A corrupted message is garbage, not a cleverly forged lie. TigerBeetle's checksums are keyless (see §3.2), so they catch accidents only. Security is out of scope by design (`safety.md`, "Security": TigerBeetle "must be running in a trusted environment").

The payoff of a weak contract: **any** transport misbehaviour that only drops, duplicates, reorders or corrupts is already inside the fault model. The consensus layer handles it. A MessageBus bug can cost liveness or performance, but to break safety it would have to forge a message with a valid 128-bit checksum and plausible contents.

### 3.2 Every message validates itself

Every message, prepare and grid block starts with the same 256-byte `Header` (`message_header.zig:14–76`):

```
 offset  field                 size
 0       checksum              u128   ← covers bytes 16..256 of the header
 16      checksum_padding      u128
 32      checksum_body         u128   ← covers the body
 48      checksum_body_padding u128
 64      nonce_reserved        u128
 80      cluster               u128   ← "binds intention": wrong cluster = invalid
 96      size                  u32    header + body
 100     epoch, view, release, protocol, command, replica, reserved_frame
 128     reserved_command      [128]u8  per-command fields (op, commit, parent, ...)
```

- `checksum` covers "only the remainder of this header. This allows the header to be trusted without having to recv() or read() the associated body" (`message_header.zig:18–21`). `set_checksum` must run *after* `set_checksum_body`, so the header checksum also covers `checksum_body`. Validating the header first, then the body, protects every byte.
- The checksum is AEGIS-128L used as a MAC with an all-zero key (`checksum.zig:12–30`). It's fast thanks to AES hardware instructions, and 128 bits wide. Being keyless, it detects corruption but doesn't authenticate.
- After the checksum passes, `invalid()` checks that every padding and reserved field is zero, that `size` is in bounds, and the per-command rules (`message_header.zig:167–200`). A header that is *valid but nonsensical* is still rejected.

On receive, `MessageBuffer` (`message_buffer.zig`) runs a small parsing state machine:

```
bytes from kernel ─► advance_header:  have 256 B? ─► header checksum OK? ─► command known?
                                                         │ no                  │ no
                                                         ▼                     ▼
                                                 invalidate(.header_checksum)  vsr.fatal(...)
                                       size in [256, message_size_max]? ─ no ─► invalidate(.header_size)
                  ─► advance_body:    have `size` bytes? ─► body checksum OK? ─ no ─► invalidate(.body_checksum)
                                                                │ yes
                                                                ▼
                                                     message is consumable
```

An invalid buffer makes the bus **terminate the connection** (`message_bus.zig:887–893`). Remaining bytes on a corrupted stream can't be trusted to frame correctly, so the whole connection is dropped and later re-established. From the protocol's point of view, a corrupted message becomes a **dropped** message, which the protocol already handles.

One deliberate exception: a header with a *valid checksum* but an *unknown command* is fatal: `"unknown VSR command, crashing for safety"` (`message_buffer.zig:139–152`). A valid checksum means a real TigerBeetle produced it, so the peer speaks a newer protocol. Guessing would be unsafe.

### 3.3 How each fault is absorbed

| Fault | Mechanism | Where |
|---|---|---|
| **Drop** | Every exchange has a timeout and retry. The primary resends a prepare to replicas that haven't acked; clients resend requests; repair requests expire and are re-sent. | `on_prepare_timeout` (`replica.zig:3629–3698`); 17 named `Timeout`s (`replica.zig:1354–1449`); `client.zig:663` |
| **Duplicate** | Quorums are **bitsets indexed by replica**, so a second `prepare_ok` from replica 3 is the same bit. Duplicate requests hit the session table (Concept 27). Duplicate JVs are detected and ignored. | `QuorumCounter = stdx.BitSetType(constants.replicas_max)` (`replica.zig:141`); `count_message_and_receive_quorum_exactly_once` (`replica.zig:4143`); JV dedup (`replica.zig:4085–4135`) |
| **Reorder** | Messages carry op numbers, views and hash-chain parents. A backup that gets op 12 before op 11 stores 12 and leaves a gap, repaired later. Nothing assumes arrival order. | `vsr.md` "Repair Journal": "receiving nonconsecutive ops, leaving a gap"; Concept 25 |
| **Corrupt** | Header + body checksums; invalid → drop the connection → becomes a drop. | `message_buffer.zig:115–186` |
| **Partition** | View change needs a quorum; asymmetric partitions can't force a view change alone (`vsr.md` "Start-View-Change"). | Concepts 21–22 |

There is no acknowledgement or sequence-number layer in the transport. Reliability is *end-to-end*: the protocol retries until it sees the effect it needs (a `prepare_ok`, a `reply`, a `block`).

### 3.4 The message pool and reference counting

All messages live in a **static pool** allocated once at startup (`message_pool.zig`):
- Each `Message` is a pointer to a `message_size_max` (1 MiB) buffer aligned to `sector_size` (4 KiB), so it can be written straight to disk with Direct IO. It also holds a `references: u32` and an intrusive free-list link (`message_pool.zig:132–136`).
- `get_message` pops the free list and sets `references = 1` (`message_pool.zig:254–271`). `ref()` increments the count. `unref()` decrements it, and at zero pushes the message back on the free list (`message_pool.zig:288–300`).
- `get_message` unwraps the pop with `.?`. If the pool is ever empty, the replica crashes. The pool size is therefore *computed* from every place a message can be held (`messages_max_replica`, `message_pool.zig:53–101`). The comment says: "There must be enough messages to ensure that the replica can always progress, to avoid deadlock."

Why reference counting? One prepare is held at the same time by:
- the pipeline,
- an in-flight journal write,
- the send queue of every replica connection,
- and maybe a repair response.

Refcounting lets all of them share **one** 1 MiB buffer with zero copies. The primary even reuses the client's request buffer as the prepare: "Reuse the Request message as a Prepare message by replacing the header" (`replica.zig:7393`).

Typed views over the same memory, such as `Message.Prepare` and `Message.Request`, are built by `CommandMessageType`. Comptime asserts check that their layout is identical to `Message`, so casts between them are free and safe (`message_pool.zig:303–329`).

Pool size, worked out from the formula for a default production replica in a 6-replica cluster. Defaults: `--limit-pipeline-requests` = `ceil(64/2) − 8 = 24` (`tigerbeetle/cli.zig:475–476`), so `pipeline_limit = 8 + 24 = 32`.

```
journal reads 8 + journal writes 32 + client-reply reads 1 + writes 2
+ grid repair reads 4 + loopback 1 + pipeline 32 + commit_prepare 1 + sync_view 1
+ JV quorum min(6,6)=6 + burst 1 + commit reply 1                      =  90
TCP: connections_max = 6 + 32 − 1 = 37 → recv buffers 37 + send queues 37×4 = 185
total ≈ 275 messages × 1 MiB ≈ 275 MiB, fixed for the life of the process
```

(This is my arithmetic from the code's formula, not a figure quoted in the docs.)

### 3.5 The MessageBus over TCP

`message_bus.zig` is deliberately thin.

**Who connects to whom:** "Each replica is responsible for connecting to replicas that come after it in the configuration. This ensures that replicas never try to connect to each other at the same time." Clients connect to all replicas (`tick_connect`, `message_bus.zig:337–350`).

**Socket options** (`init_tcp`, `message_bus.zig:269–284`; values from `config.zig:107–114`, `constants.zig:433–484`):

| Option | Value |
|---|---|
| `TCP_NODELAY` | on (no Nagle batching; TigerBeetle batches itself, Concept 7) |
| keepalive | idle 5 s, interval 4 s, count 3 |
| `TCP_USER_TIMEOUT` | `(5 + 4×3) × 1000 = 17,000 ms` |
| `SO_RCVBUF` | 4 MiB |
| `SO_SNDBUF` | replica: `4 × 1 MiB`; client: `2 × 1 MiB` |

**Bounded send queues.** `connection_send_queue_max_replica = @max(@min(clients_max, 4), 2)` = 4, and 2 for clients (`constants.zig:420–424`). When the queue is full, `send_message` **drops** the message with "queue full, dropping" (`message_bus.zig:950–956`). This is fine by contract.

**Silent drops elsewhere too.** If there's no connection to the target replica, the message is logged and dropped (`message_bus.zig:913–921`). For clients: "If the client is not currently connected, the message is silently dropped" (`message_bus.zig:925`).

**Any error terminates.** A send error (`send_callback`, `message_bus.zig:1036–1045`) or invalid received bytes close the connection. Reconnects use exponential backoff with jitter between `connection_delay_min = 50 ms` and `connection_delay_max = 1000 ms` (`config.zig:106–107`).

**Unknown peers.** An accepted connection must identify itself (replica or client) within `message_bus_unknown_time_to_live = 45 s` (`constants.zig:747`), or it may be reclaimed. When connections run out, `reclaim_connection` evicts, in order: an unknown peer past its TTL, then any client, then any unknown peer (`message_bus.zig:459–513`).

**Zero-copy receive.** `MessageBuffer` reads into a pool `Message`. "If a recv syscall reads exactly one message, no copying occurs" (`message_buffer.zig:22–23`).

### 3.6 How the simulator injects network faults

In VOPR (Concept 29), the real `MessageBus` is replaced by `testing/cluster/message_bus.zig` plus a simulated `Network` built on `PacketSimulator`. Each directed path (source → target) is a **priority queue keyed by delivery time**:

```
send_message ─► Network.send_message: copy into a network-owned Message
             ─► PacketSimulator.submit_packet(path):
                  if queue.len + 1 > path_maximum_capacity: drop a RANDOM queued packet
                  ready_at = now + max(one_way_delay_min, Exp(one_way_delay_mean))
tick/step    ─► for each path not clogged: pop packets with ready_at <= now
                  submit_packet_finish:
                    link filter says drop (partition)?  → drop
                    chance(packet_loss_probability)?    → drop
                    chance(packet_replay_probability)?  → re-submit a clone (DUPLICATE)
                    deliver ─► Network.packet_deliver:
                                 target bus down?       → drop
                                 target buffer full?    → drop
```

Every fault on the list falls out of this design:
- **Reordering** happens naturally: delays are random (exponential), so a later packet often overtakes an earlier one.
- **Duplicates** come from replay; the clone gets a fresh random delay.
- **Clogging** blocks a path for an exponentially distributed time (`tick`, `packet_simulator.zig:387–423`).
- **Partitions** come in three modes, `uniform_size`, `uniform_partition` and `isolate_single`, either `symmetric` or `asymmetric` (`packet_simulator.zig:60–80`). Per-command link filters let tests drop, say, only `prepare_ok`.

VOPR picks these per seed (`vopr.zig:444–464`):

| Knob | Range per run |
|---|---|
| `one_way_delay_min` | 0–30 ms |
| `one_way_delay_mean` | 30–100 ms |
| `packet_loss_probability` | 0–30% |
| `packet_replay_probability` | 0–50% |
| `path_maximum_capacity` | 2–20 packets |
| `path_clog_probability` | 0–2% per tick |
| `path_clog_duration_mean` | 0–5,000 ms |
| `partition_probability` | 0–3% per tick |
| `unpartition_probability` | 1–10% per tick |

> ⚠️ **Docs vs code.** `ARCHITECTURE.md` says TigerBeetle assumes messages may be **corrupted**, but the VOPR packet simulator never corrupts a packet. It only drops, delays, duplicates, reorders and partitions. Wire corruption is tested separately: `message_bus_fuzz.zig` runs the *real* MessageBus with `send_corrupt_probability` up to 10%, flipping a byte in the send buffer (`message_bus_fuzz.zig:94, 661–667`). Storage corruption is injected by the simulated storage (Concept 29). The claim holds overall, just not through the packet simulator.

### 3.7 The 8 fallacies, mapped

The list is the standard one (L. Peter Deutsch and others at Sun); the mapping to TigerBeetle mechanisms is this lesson's.

| # | Fallacy | TigerBeetle's answer |
|---|---|---|
| 1 | The network is reliable | Drops, dups, reorders are in the contract; end-to-end retries |
| 2 | Latency is zero | RTT-adaptive timeouts with backoff (`rtt`, `rtt_multiple`, `backoff_*`, `constants.zig:729–741`); flexible quorums wait only for the fastest replicas (Concepts 22–23) |
| 3 | Bandwidth is infinite | Bounded send queues; batching (Concept 7); repair budgets (Concept 25) |
| 4 | The network is secure | Out of scope: trusted environment; checksums catch accidents only |
| 5 | Topology doesn't change | Star replication tolerates path changes (`ARCHITECTURE.md`); clients hedge to a backup |
| 6 | There is one administrator | The `cluster` ID in every header rejects cross-cluster traffic (staging vs production) |
| 7 | Transport cost is zero | Zero-copy refcounted messages; one checksum pass per message |
| 8 | The network is homogeneous | Transport-agnostic protocol; fixed `extern struct` wire format; the build assumes little-endian (`config.zig:204`) |

## 4. What it means for ashi

**Design the protocol against a weak contract first, then pick a transport.** If ashi's consensus is correct over "drop / dup / reorder / corrupt", the transport choice is a performance decision, not a correctness one.

**Rust features that help:**
- `#[repr(C, align(16))] struct Header` with `const _: () = assert!(size_of::<Header>() == 256)`. Cast received bytes with `zerocopy::FromBytes` or `bytemuck::Pod` *after* the checksum passes.
- Enums from the wire: read `command` as a `u8` and `TryFrom` into the enum. Never transmute an unchecked byte into a Rust `enum`; that is UB. TigerBeetle has the same hazard and handles it with `std.enums.fromInt`.
- A `MessageBus` trait with two implementations, real and simulated, used through generics (monomorphized), not `Box<dyn ..>`. That mirrors TigerBeetle's `comptime` `MessageBus` parameter.

**Reference counting in Rust:**
- `Rc<[u8]>` / `Arc` / `bytes::Bytes` each heap-allocate per message, which breaks static allocation. Instead build an index-based pool: `struct MessageRef(u16)` into a `Vec<Slot { refs: u32, buf: Box<AlignedBuf> }>`. Keep explicit `ref_`/`unref` calls, or a guard type whose `Drop` calls `pool.unref`. Since everything is single-threaded (Concept 6), `Cell<u32>` suffices; no atomics.
- Allocate 1 MiB aligned buffers with `std::alloc::alloc_zeroed(Layout::from_size_align(1 << 20, 4096))`. `Box::new([0u8; 1 << 20])` can overflow the stack in debug builds.
- Compute the pool size from a formula, as TigerBeetle does, and fail at startup, not at runtime.

**Transport crates:** `io-uring` (low-level, close to TigerBeetle), `mio` (epoll/kqueue readiness), `socket2` (sets `TCP_USER_TIMEOUT`, keepalive, buffer sizes that `std::net` doesn't expose). `tokio` works but brings its own scheduler and allocations per task. If used, keep it at the edge and never inside the deterministic core. TigerBeetle has zero dependencies, including its own AEGIS checksum; the `aegis` crate exists if ashi wants the same checksum without writing it.

**Pitfalls:**
- Don't add a "reliable delivery" layer (acks, sequence numbers) under consensus "to be safe". It duplicates what the protocol already does, and it hides faults from the simulator.
- Don't treat a full send queue as backpressure that blocks the core. Drop, and let the protocol retry.
- Make sure the simulator *can* corrupt, either in the packet simulator or a separate bus fuzzer. Otherwise checksum-handling code is untested.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt** the weak contract, self-validating headers and the static refcounted pool. **Adapt** the transport: a thin `mio` or `io-uring` bus with the same drop-on-full and terminate-on-invalid rules.

## 5. Drill questions

### Q1. Recall

List the four network faults TigerBeetle's contract assumes. Which word qualifies "corrupted", and what does it rule out?

> _Your answer:_

### Q2. Mechanism

Explain why a replica that receives a message with a bad body checksum terminates the *whole TCP connection* instead of skipping that one message. And why a message with a *valid* checksum but unknown command crashes the replica instead.

> _Your answer:_

### Q3. Back-of-envelope

(a) Compute `TCP_USER_TIMEOUT` from the keepalive constants.
(b) A primary sends one 1 MiB prepare to 5 backups. How many 1 MiB buffers does that use, thanks to refcounting? How many would a "copy per destination" design use?
(c) In a VOPR run with `packet_loss_probability = 30%` and no replay, what is the probability that a request *and* its reply both get through on one attempt? How many attempts are expected per successful round trip?

> _Your answer:_

### Q4. What breaks if…

…quorums were counted with an integer counter (`acks += 1` per `prepare_ok`) instead of a bitset indexed by replica?

> _Your answer:_

### Q5. ashi design (open-ended)

ashi could use QUIC instead of TCP: multiple streams, built-in encryption, connection migration. Under TigerBeetle's weak contract, what would QUIC actually buy ashi, what would it cost (dependencies, determinism, CPU), and would it change any consensus code?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** Dropped, duplicated, reordered, corrupted. "**Non-byzantine**": corruption is accidental, never a crafted, valid-looking lie from a malicious node. TigerBeetle's checksums are keyless, so they detect accidents, not forgeries.

**Q2.** A TCP stream has no message boundaries of its own; the `size` field in each header frames the next message. Once bytes are known corrupt, TigerBeetle can't trust that the framing that follows is aligned. The safe move is to drop everything on that stream and reconnect. Under the weak contract that just turns into dropped messages, which the protocol retries. A *valid* checksum with an *unknown* command means the bytes are genuine: a real TigerBeetle produced them. So the peer runs a protocol this binary doesn't understand. Dropping silently could hide an upgrade mismatch, and guessing could be unsafe. So it is `vsr.fatal(.unknown_vsr_command, "... crashing for safety")`.

**Q3.** (a) `(tcp_keepidle + tcp_keepintvl × tcp_keepcnt) × 1000 = (5 + 4×3) × 1000 = 17,000 ms`. (b) **One** buffer, referenced by the pipeline, the journal write and 5 send queues (references ≈ 7). Copy-per-destination would need 6 or more, which is also 6× the memcpy bandwidth for a 1 MiB message. (c) Each leg survives with 0.7, so both legs survive with 0.7 × 0.7 = **0.49**. The number of attempts is geometric with mean 1/0.49 ≈ **2.04**. This ignores clogging, capacity drops, and the hedged copy to a backup, which helps.

**Q4.** Under duplication, a single replica's `prepare_ok`, delivered twice (packet replay, or a retransmit after `on_prepare_timeout`), would count twice. With 6 replicas (quorum 3): the primary's own ack plus a duplicated ack from replica 1 gives "3" while only 2 replicas actually have the prepare durably. The primary would commit and reply. If the primary and replica 1 then fail, the committed op exists nowhere else, and a view change can truncate it via nacks (Concept 24). A committed transfer is lost. The bitset makes acks idempotent per replica.

**Q5.** Open-ended. QUIC would buy encryption in transit (fallacy 4, which TigerBeetle leaves to the environment), per-stream ordering without head-of-line blocking between unrelated messages, and connection migration. Under the weak contract none of these change consensus code: ordering and reliability aren't relied on anyway. Costs: a large dependency (`quinn`/`rustls`), userspace congestion control and crypto on the core's CPU budget, and more moving parts for the simulator to model. The likely conclusion: the consensus layer stays the same, and the transport becomes a deployment option. If encryption is needed, a TLS terminator or WireGuard tunnel at the edge may be cheaper.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
