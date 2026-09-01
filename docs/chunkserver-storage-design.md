# Moat Chunk Store: Storage Layout & Space Management Design<br>Moat Chunk Store：存储布局与空间管理设计

- Status: Draft
- Scope: Single-node (single-device) storage model and allocator. Distributed protocols, replication/EC implementations, and cache policies (admission / SIEVE / eviction) are out of scope — but clear interface boundaries are reserved for them.
- 范围：单机（单盘）存储模型与 allocator。不含分布式协议、replication/EC 的实现、cache 策略（admission / SIEVE / eviction）的实现——但为它们预留清晰的接口边界。

## 1. Goals & Non-Goals · 目标与非目标

### 1.1 Goals · 目标

1. **A general-purpose variable-length KV / chunk storage engine**: both keys and values are variable-length; the upper layer can build either a disk cache or a chunkserver on top (a chunk is just an object with versioning semantics).

   **通用变长 KV / chunk 存储引擎**：key 与 value 均变长；上层既可以做磁盘 cache，也可以做 chunkserver（chunk 即一种带 version 语义的 object）。

2. **Operate directly on raw block devices**: no filesystem dependency, avoiding inode locks, extent trees, `fallocate` / punch-hole, and the write amplification and unpredictable latency of journaling filesystems. The ability to emulate a device with a large file is kept for testing.

   **直接使用 raw block device**：不依赖文件系统，规避 inode lock、extent tree、`fallocate` / punch-hole、日志型 FS 的写放大与不可控延迟。同时保留"以大文件模拟设备"的能力用于测试。

3. **Metadata separated from data**: metadata is compactly packed and records data locations; the data region carries no lookup responsibility (but keeps self-describing headers for verification and recovery).

   **metadata 与 data 分离**：metadata 紧凑排列，记录数据位置；数据区不承担查找职责（但保留自描述头用于校验与恢复）。

4. **Append-only / copy-on-write data**: values are never overwritten in place; an old location may be reused only after it is confirmed to have no in-flight references.

   **数据 append-only / copy-on-write**：不原地改写 value；旧位置在确认无在途引用后才可复用。

5. **Reclamation without data movement**: reclaim in whole slots / groups. The cache scenario reclaims by evicting entire groups; the durable scenario avoids data-movement GC via slot reuse, with migrating compaction only as an optional low-priority background operation.

   **回收不搬数据**：以完整 slot / group 为单位回收。cache 场景通过整 group 淘汰完成回收；durable 场景通过 slot 复用避免搬迁 GC，搬迁式 compaction 仅作为可选的后台低优先级操作。

6. **Control four classes of overhead**: internal/external fragmentation, metadata footprint, I/O segment count per request (target: always 1), and random I/O plus software-stack overhead (io_uring fixed buffers, hugepages, avoiding iou-wrk punts).

   **控制四类开销**：内部/外部碎片、metadata 占用、单请求 I/O segment 数（目标恒为 1）、随机 I/O 与软件栈开销（io_uring 固定缓冲、hugepage、避免 iou-wrk punt）。

7. **Decouple logical allocation granularity from NVMe I/O size**: the allocation unit is determined by size classes, while the I/O layer independently chooses request sizes across 4/16/64/128 KiB windows.

   **逻辑分配粒度与 NVMe I/O size 解耦**：分配单元由 size class 决定，I/O 请求大小由 I/O 层按 4/16/64/128 KiB 等窗口独立选择。

### 1.2 Non-Goals · 非目标（本文不展开）

- Distributed membership, chain replication / EC encoding themselves (the engine only guarantees the required primitives, see §10).
- Cache admission / eviction policies themselves (only the interfaces the engine exposes to the policy layer are defined, see §7).
- Load balancing across devices (one independent engine instance per device; multi-device composition happens above).

---

- 分布式成员管理、chain replication / EC 编码本身（只保证引擎提供所需原语，见 §10）。
- cache admission / eviction 策略本身（只定义引擎暴露给策略层的接口，见 §7）。
- 多盘间的负载均衡（每个设备一个独立 engine 实例，多盘由上层组合）。

## 2. Overall Model · 总体模型

```
┌─────────────────────────────────────────────────────────────┐
│  Upper layer 上层: cache (admission/SIEVE/eviction) /        │
│        chunkserver (replication, EC, version semantics)      │
├─────────────────────────────────────────────────────────────┤
│  Object layer 对象层: KV record encoding, index              │
│        (key → location), meta persistence                    │
│        (journal / snapshot, two profiles)                    │
├─────────────────────────────────────────────────────────────┤
│  Space layer 空间层 (allocator): group state machine,        │
│        size classes, slab bitmap / log bump, pin & reclaim   │
├─────────────────────────────────────────────────────────────┤
│  Device layer 设备层: raw block device, io_uring + O_DIRECT, │
│        fixed buffer arena (hugepage, NUMA)                   │
└─────────────────────────────────────────────────────────────┘
```

The allocator is strictly decoupled from the cache: the Space layer only provides "allocate / free / whole-group lifecycle / watermark notification" and knows nothing about SIEVE, TTL, or admission; the policy layer drives whole-group eviction through the interface in §7.2.

allocator 与 cache 严格解耦：Space 层只提供「分配 / 释放 / 整 group 生命周期 / 水位通知」，不知道 SIEVE、TTL、admission 的存在；策略层通过 §7.2 的接口驱动整 group 淘汰。

A note on the central difficulty: fixed-length storage enjoys zero fragmentation, trivial allocation, and in-place slot reuse for free — variable length destroys all three at once. This design's core move is to re-obtain those properties through controlled "length fixing": a configurable size class table with provable fragmentation bounds, plus two group forms with different reclamation granularities, instead of a hard-coded record size.

关于核心难点的说明：定长存储天然享有零碎片、O(1) 分配与 slot 原地复用——变长会同时摧毁这三者。本设计的核心动作是通过受控的"定长化"重新获得这些性质：可配置、有可证明碎片上界的 size class 表，加上两种回收粒度不同的 group 形态，而不是写死一个记录尺寸。

## 3. Device Layout · 设备布局

```
LBA 0
┌──────────────┬──────────────┬───────────────┬─────────────────────────────┬──────────────┐
│ Superblock A │ Group Table  │  Meta Region  │  Data Region                │ Superblock B │
│   (4 KiB)    │  (A/B x2)    │ (journal +    │  = Group[0..N)              │   (4 KiB)    │
│              │              │  checkpoint)  │  each group fixed size G    │              │
└──────────────┴──────────────┴───────────────┴─────────────────────────────┴──────────────┘
```

All regions are aligned to 4 KiB (`BLOCK`, the device logical block). The device geometry is fully determined by a handful of parameters in the superblock; every location is derivable by pure arithmetic, with no extra lookup structure.

所有区域 4 KiB（`BLOCK`，设备逻辑块）对齐。设备几何完全由 superblock 中少数参数决定，任何位置都可用纯算术求出，不需要额外查找结构。

### 3.1 Superblock

One copy at each end of the device (A/B), each with an independent CRC and a monotonic epoch; updates alternate between the two, and reads pick the copy with the larger epoch and a valid CRC. Contents:

- magic, format version, device UUID (guards against grabbing the wrong disk / reordered devices; identify disks by stable identifiers such as the NVMe serial + namespace rather than `/dev/nvmeXn1`, which may renumber across reboots);
- geometry: `BLOCK` (4 KiB), group size `G`, data region start, group count N;
- the size class table (explicitly persisted, customizable per format, including exact classes, see §4.2);
- location and size of the meta region, meta profile (cache / durable);
- format timestamp, salt (for key-hash collision-attack resistance — required for general-purpose storage).

首尾各一份（A/B），带独立 CRC 与单调 epoch，更新时交替写、读取时取 epoch 大且 CRC 正确的一份。内容：

- magic、format version、device UUID（防止拿错盘/错序；用 NVMe serial + namespace 等稳定标识识别设备，而非重启后可能重排的 `/dev/nvmeXn1`）；
- 几何参数：`BLOCK`（4 KiB）、group size `G`、data region 起始、group 数 N；
- size class 表（显式持久化，允许每次 format 定制，含 exact class，见 §4.2）；
- meta region 的位置与大小、meta profile（cache / durable）；
- format 时间、salt（用于 key hash 抗碰撞攻击，通用存储需要）。

### 3.2 Group Table

An array of `N` fixed-size group descriptors (16 B each: state, class_id, seal_seqno, live_count summary; a CRC covers the whole table), written as a whole with A/B rotation (with a 30 TiB disk and 256 MiB groups, N ≈ 120 K entries ≈ 2 MiB — one negligible sequential write). It only needs to be **eventually consistent**: the authoritative state is reconstructible from the meta region plus the self-describing data inside groups; the group table is merely a recovery-acceleration hint — it is allowed to lag, backstopped by the self-describing data region.

`N` 个定长 group descriptor 的数组（每项 16 B：state、class_id、seal_seqno、live_count 摘要、CRC 覆盖整表），A/B 双份轮转整表写入（N 在 30 TiB 盘、256 MiB group 下约 12 万项 ≈ 2 MiB，一次顺序写可忽略）。它只需**最终一致**：真实的权威状态可由 meta region + group 内自描述数据重建，group table 只是加速恢复的 hint——允许落后，靠数据区自描述兜底。

### 3.3 Data Region & Groups · Data Region 与 Group

- **Groups are fixed-size** (default 256 MiB, a format parameter, tunable 64 MiB–1 GiB); `group_id → LBA` is a pure multiplication.
- A group is **the largest unit of the space lifecycle**: pre-allocation, sealing, reclamation, eviction, and quarantine all take group boundaries.
- The binding between a group and a size class is **dynamic**: a FREE group can be claimed by any class and returns to the common pool once EMPTY. This avoids the external fragmentation that static partitioning suffers when the workload's class distribution drifts.
- Fixed-size groups deliberately align with the SSD's internal large write units: writes within one group are sequential or nearly so (see §4.6, one open group per writer), reducing device-side GC pressure.

---

- **Group 是定长的**（默认 256 MiB，格式化参数，64 MiB–1 GiB 可调），`group_id → LBA` 为纯乘法。
- Group 是**空间生命周期的最大单位**：预分配、密封（seal）、回收、淘汰、quarantine 都以 group 为边界。
- Group 与 size class 的绑定是**动态的**：FREE group 可以被任何 class 认领，EMPTY 后归还公共池。这避免了静态分区在负载 class 分布漂移时的外部碎片。
- 定长 group 有意对齐 SSD 内部的大粒度写单元：同一 group 内的写是顺序或近似顺序的（见 §4.6 每 writer 独占 open group），降低设备 GC 压力。

Group state machine · Group 状态机：

```
      claim (bind class)            full / explicit seal
      认领(bind class)              写满/主动 seal
 FREE ────────────────▶ OPEN ──────────────────────▶ SEALED
   ▲                      │                             │
   │                      │ (slab) all slots freed      │ (slab) slots drain to empty
   │                      │ (slab) slot 全部释放          │ (slab) slot 陆续释放至空
   │                      ▼                             ▼
   └───────────────── EMPTY ◀──────────── QUARANTINE ◀──┘
        (return to pool after pins drain & fence passes)  (whole-group eviction/delete)
        (等 pin 清零、fence 通过后归还公共池)                (整 group 淘汰/删除时)
```

## 4. Space Allocation · 空间分配

### 4.1 Design Space Evaluation · 设计空间评估

Four candidates, evaluated one by one (object size S, allocation block B):

**(a) Uniform large blocks + bitmap, objects spanning possibly non-contiguous blocks.**
Metadata needs a per-object block list (essentially an extent list), and reading one object may require multiple I/O segments / iovecs — directly violating both the "segment count = 1" and "compact metadata" goals. **Rejected.**

**(b) Extents (variable-length contiguous allocation, best-fit / first-fit).**
Minimal internal fragmentation, but classic external fragmentation: after long-term operation, large contiguous space runs out, forcing migrating compaction to keep large objects allocatable — violating "no data-movement GC". The buddy variant can merge, but requires power-of-two rounding (equivalent to the coarsest size classes, worst-case 50% internal fragmentation) and neighbor merging introduces cross-object metadata coupling. **Rejected** (recorded as a baseline for comparison).

**(c) Size class + group bitmap (slab).**
Each group binds to one class; slots are fixed-size and contiguous; the bitmap has one bit per slot:

- objects are contiguous by construction → reads and writes are always 1 segment, 1 iovec;
- free = clear a bit, reuse = set a bit, no data movement;
- internal fragmentation has a provable bound, determined by class spacing (§4.2);
- external fragmentation manifests as "half-empty groups of the same class", bounded by ≈ active class count × G (at most a constant number of OPEN groups per class), and dynamic class↔group binding makes it self-healing as workloads shift;
- bitmap cost is negligible: with 256 MiB groups and a minimum 4 KiB slot, 64 Ki bits = 8 KiB per group, amortizing to 32 B per ~1 GiB of data.

**(d) Log groups (bump allocation)**, as a complement to (c): append-only contiguous writes within a group, no slot concept, **reclaimable only as a whole group**. Zero internal fragmentation (optional record-level 4 KiB alignment only), writes are always large sequential I/O; the cost is no per-object space reuse.

**Conclusion: extents are not needed. Adopt (c) + (d), dual-form groups**, routing by object size and use case:

四个候选逐一评估（对象大小记 S，分配块记 B）：

**(a) 统一大 block + bitmap，对象占多个可能不连续的 block。**
metadata 需要 per-object block 列表（本质是 extent list），读一个对象可能需要多个 I/O segment / iovec —— 直接违反「segment 数 = 1」与「metadata 紧凑」两条目标。**否决。**

**(b) extent（变长连续分配，best-fit / first-fit）。**
内部碎片最小，但产生经典外部碎片：长期运行后连续大空间枯竭，必须做搬迁 compaction 才能维持大对象可分配 —— 违反「不做数据搬迁 GC」。buddy 变体可合并，但要求 2 的幂圆整（等价于最粗的 size class，最坏 50% 内部碎片）且相邻块合并引入跨对象的元数据耦合。**否决**（作为对照记录在案）。

**(c) size class + group bitmap（slab）。**
每个 group 绑定一个 class，slot 定长连续，bitmap 一 bit 一 slot：

- 对象天然连续 → 读写恒为 1 个 segment、1 个 iovec；
- 释放 = 清 bit，复用 = 置 bit，无搬迁；
- 内部碎片有可证明上界，由 class 间距决定（§4.2）；
- 外部碎片表现为「同 class 的半空 group」，上界 ≈ 活跃 class 数 × G（每 class 至多常数个 OPEN group），且 class 与 group 动态绑定使其随负载自愈；
- bitmap 成本可忽略：256 MiB group、最小 4 KiB slot 时 64 Ki bit = 8 KiB/group，全盘 ~1 GiB 数据摊 32 B。

**(d) log group（bump 分配）**，作为 (c) 的补充：group 内 append-only 连续追加，无 slot 概念，**只能整 group 回收**。零内部碎片（仅记录级 4 KiB 对齐可选），写恒为顺序大 I/O，代价是无法 per-object 复用空间。

**结论：不需要 extent。采用 (c) + (d) 双形态 group**，按对象大小与使用场景路由：

| | slab group | log group |
|---|---|---|
| Allocation 分配 | class bitmap, O(1) | bump pointer, O(1) |
| Reclaim granularity 回收粒度 | single slot, no movement 单 slot（无搬迁） | whole group 整 group |
| Internal fragmentation 内部碎片 | ≤ bound set by class spacing ≤ class 间距决定的上界 | ~0 (alignment loss only 仅对齐损耗) |
| Best for 适用 | durable (chunkserver); large cached objects durable（chunkserver）；大对象 cache | small cached objects (whole-group eviction = reclamation) cache 小对象（整 group 淘汰即回收）；批量写入 |
| Write pattern 写模式 | one contiguous write per slot slot 内一次连续写 | pure sequential append (write-combine) 纯顺序追加（write-combine） |

This maps exactly onto the two upper layers: **chunkservers use slab** (delete → clear bit → reuse, never move data); **caches use log for small objects** (evicting a whole group completes reclamation, naturally isomorphic to group-granular eviction policies). Large cached objects can also go through slab with per-object eviction by the policy layer.

这正对应两种上层：**chunkserver 用 slab**（删除→清 bit→复用，永不搬数据）；**cache 小对象用 log**（淘汰整 group 即回收，与「以 group 为单位淘汰」的策略天然同构）。cache 的大对象也可走 slab 并由策略层做 per-object 淘汰。

### 4.2 Size Class Table · Size class 表

- Class sizes must be integer multiples of 4 KiB (`BLOCK`) (guaranteeing LBA-aligned slot start addresses and O_DIRECT usability); **powers of two are not required**.
- Default table: **4 classes per doubling** (adjacent ratio ≈ 1.19) from 4 KiB to 4 MiB (40 classes); from 4 MiB to 64 MiB, reduced to **2 classes per doubling** (8 classes — large objects are few and absolute waste dominates; fewer classes bound OPEN-group usage). ~48 classes total.
  - Internal fragmentation bounds: worst ~16% / mean ~8% under a uniform distribution for 4-per-doubling; worst ~29% for 2-per-doubling.
  - OPEN-group space bound: 48 classes × 256 MiB ≈ 12 GiB (lazy binding keeps actual usage far lower; small devices mitigate with smaller G).
- **Exact classes**: classes of precise sizes can be registered at format time or at runtime (e.g. a workload whose values are all one known byte size, or EC shards of a fixed stripe size). Internal fragmentation drops to zero for known fixed-length workloads — any number of exact classes coexist with the variable-length classes.
- Maximum object size = G (an object never crosses a group). Chunkserver chunk sizes are already bounded by the upper-layer protocol (e.g. ≤ 64 MiB), so this is not a limitation; larger objects are chunked by the upper layer — which is a chunkserver's job in the first place.

---

- class 值必须是 4 KiB（`BLOCK`）的整倍数（保证 slot 起址 LBA 对齐、O_DIRECT 可用），**不要求 2 的幂**。
- 默认表：**每倍频 4 级**（相邻比 ≈ 1.19），范围 4 KiB → 4 MiB（40 class）；4 MiB → 64 MiB 降为**每倍频 2 级**（8 class，大对象数量少、绝对浪费才是主要矛盾，减 class 数控制 OPEN group 占用）。共约 48 class。
  - 内部碎片上界：每倍频 4 级最坏 ~16%、均匀分布下均值 ~8%；每倍频 2 级最坏 ~29%。
  - OPEN group 空间上界：48 class × 256 MiB ≈ 12 GiB（惰性绑定，实际远小于此；小容量设备用小 G 缓解）。
- **exact class**：format 或运行时可注册精确尺寸的 class（例如全部 value 为某已知字节数的负载、或定长条带的 EC shard）。对已知定长负载内部碎片归零——任意多个定长 class 与变长 class 共存。
- 对象尺寸上限 = G（一个对象不跨 group）。chunkserver 的 chunk 尺寸本就由上层协议约定（如 ≤ 64 MiB），不构成限制；更大的对象由上层切 chunk —— 这本来就是 chunkserver 的职责。

### 4.3 Slab Group

- Slot count = ⌊G / class⌋; trailing space smaller than one slot is abandoned (≤ class/G, negligible).
- One bitmap per group (an in-memory `AtomicU64` word array with CAS allocation; no persistence needed — rebuilt on recovery from meta / seal ToC, see §9).
- Each slot carries an in-memory `SlotMeta`: `state` (Empty/Writing/Live/Evicting/Retired), `generation` (incremented on every reuse, ABA guard), `pin_count` (in-flight read references). All in-memory, amortized into index cost.
- **Slack between slot capacity and record length is allowed and useful**: append-style chunks on a chunkserver can do in-place tail appends within the already-allocated capacity (a v2 feature); logically this is still COW that never crosses committed data.

---

- slot 数 = ⌊G / class⌋，尾部不足一个 slot 的空间放弃（≤ class/G，可忽略）。
- 每 group 一个 bitmap（内存中 `AtomicU64` 字数组，CAS 分配；持久化不需要——恢复时由 meta / seal ToC 重建，见 §9）。
- 每 slot 附带内存态 `SlotMeta`：`state`（Empty/Writing/Live/Evicting/Retired）、`generation`（每次复用 +1，ABA 防护）、`pin_count`（在途读引用）。这些均为内存结构，摊入索引成本。
- **slot 容量 > 记录长度的富余是允许且有用的**：chunkserver 的 append 型 chunk 可在已分配容量内做 in-place 尾部追加（v2 特性），逻辑上仍是"未越过已提交数据的 COW"。

### 4.4 Log Group

- Only a bump pointer plus a seal position; records are appended contiguously (record start aligned to `BLOCK` by default, so any record is readable as a single aligned segment; fully packed layout trading read amplification for zero fragmentation is available as a class parameter).
- The write path naturally write-combines: writers batch records in a fixed buffer (default 512 KiB flush window) and land many records with one sequential write — small objects no longer produce small random writes.
- On seal, a **ToC (table of contents) footer block** is written: all (key_hash, offset, len, seqno) of the group, so recovery reads the ToC instead of scanning the whole group.
- Reclamation is whole-group only: the policy layer evicts every object in the group (§7.2), or in durable scenarios a background task **rewrites the surviving records of sparse log groups as new records** (the only place data is ever moved, and it is optional and off by default).

---

- 仅 bump pointer + seal 位置两个游标；记录连续追加（默认按 `BLOCK` 对齐记录起点，保证任意记录单段对齐可读；亦可完全紧凑 + 读放大换零碎片，作为 class 参数）。
- 写路径天然 write-combine：writer 在 fixed buffer 中攒批（默认 512 KiB flush 窗口），一次顺序写落多条记录 —— 小对象不再产生小随机写。
- seal 时写入 **ToC（table of contents）尾块**：记录本 group 内全部 (key_hash, offset, len, seqno)，恢复时只读 ToC 而非全 group 扫描。
- 回收仅整 group：策略层淘汰该 group 全部对象（§7.2），或 durable 场景由后台把稀疏 log group 的存活记录**重写为新记录**（这是唯一允许搬数据的地方，且是可选项，默认关闭）。

### 4.5 Inline & Routing · Inline 与路由

- value ≤ inline threshold (default 512 B, configurable): stored inline in meta, occupying no data-region space; reads cost 0 I/O.
- inline threshold < size ≤ small threshold (default 64 KiB, configurable): routed to log groups by default (cache profile) or the smallest fitting slab class (durable profile).
- Everything else → the smallest fitting slab class (exact-class exact matches take priority).

---

- value ≤ inline 阈值（默认 512 B，可配）：直接内联进 meta，不占数据区，读 0 次 I/O。
- inline 阈值 < size ≤ small 阈值（默认 64 KiB，可配）：默认路由 log group（cache profile）或最小适配 slab class（durable profile）。
- 其余 → 最小适配 slab class（含 exact class 精确匹配优先）。

### 4.6 Allocator Interface & Concurrency · 分配器接口与并发

```rust
trait SpaceAllocator {
    /// Returns (Location, generation). Location packs into a u64, see §5.2.
    /// 返回 (Location, generation)。Location 打包为 u64，见 §5.2。
    fn alloc(&self, class: ClassId) -> Result<Slot>;
    /// Clear bit / mark tombstone. Caller guarantees no in-flight references (§7.1).
    /// 清 bit / 标记 tombstone。调用方保证已无在途引用（§7.1）。
    fn dealloc(&self, slot: Slot);
    /// Whole-group lifecycle (driven by the policy layer).
    /// 整 group 生命周期（策略层驱动）。
    fn seal(&self, group: GroupId);
    fn retire_group(&self, group: GroupId) -> QuarantineTicket;
    /// Watermark: notify the upper layer when free groups fall below threshold
    /// (cache → trigger eviction; durable → ENOSPC early warning).
    /// 水位：空闲 group 低于阈值时通知上层（cache→触发淘汰，durable→ENOSPC 预警）。
    fn watermark(&self) -> Watermark;
}
```

- **Each writer thread exclusively owns one OPEN group per class** ("group ownership"): allocation is contention-free, and writes to a group come from a single thread → near-sequential writes. Falls back to a locked global pool only on thread exit / rare contention.
- The background keeps `min..max` FREE groups pre-provisioned per class; foreground alloc never hits the slow path (no allocation-time latency spikes).
- Globally: a `free_groups` pool (lock-free stack) + per-class OPEN lists + per-disk in-flight byte-window backpressure (separate read/write windows, reads prioritized; see §8).

---

- **每 writer 线程每 class 独占一个 OPEN group**（"group ownership"）：分配无锁竞争，且同一 group 的写入来自单一线程 → 近似顺序写。线程退出/罕见争抢时才回退到全局池加锁。
- 后台维持每 class `min..max` 个 FREE group 预备，前台 alloc 永不触发慢路径（无分配期延迟毛刺）。
- 全局：`free_groups` 池（无锁栈）+ per-class OPEN 列表 + per-disk in-flight byte window 背压（读写分窗，读优先；见 §8）。

## 5. Record Format & Metadata · 记录格式与 Metadata

### 5.1 Data-Region Records (self-describing, the recovery & verification backstop) · 数据区记录（自描述，恢复与校验兜底）

```
┌────────────────────────────────────────────────┬─────────┬──────────────┐
│ RecordHeader (64 B)                            │ key     │ value        │
│  magic u32 | flags u16 | key_len u16           │         │              │
│  value_len u32 | class u16 | reserved u16      │         │              │
│  seqno u64    (globally monotonic; recovery    │         │              │
│                dedup arbiter 全局单调，恢复仲裁)  │         │              │
│  version u64  (chunkserver: chunk_ver)         │         │              │
│  key_hash u128 (identity check without reading │         │              │
│                 the key 无需读 key 即可核对身份)  │         │              │
│  header_crc u32 | value_crc u32 (CRC32C)       │         │              │
└────────────────────────────────────────────────┴─────────┴──────────────┘
```

- header + key + value are **written in one contiguous write** (no torn-write window); reads verify `key_hash` (and the full key when necessary) plus CRC — a mismatch is treated as a miss and the index entry is repaired. This is the correctness backstop when metadata lags behind data.
- **Fragment checksums**: one CRC32C per 64 KiB fragment of the value (stored in meta or a record footer, enabled for large objects only), supporting verified partial reads on chunkservers and incremental updates via CRC combination.
- `seqno` is globally monotonic: when crash recovery finds multiple copies of a key, the larger seqno wins.

---

- header + key + value **一次连续写入**（无 torn-write 窗口）；读取时校验 `key_hash`（必要时校验完整 key）与 CRC，不匹配按 miss 处理并修复索引 —— 这是 metadata 落后于数据时的正确性兜底。
- **fragment checksum**：value 按 64 KiB fragment 各存一个 CRC32C（放 meta 或记录尾部，仅大对象启用），支持 chunkserver 的部分读校验与 CRC 合并式增量更新。
- `seqno` 全局单调：崩溃恢复扫到同 key 多份时以 seqno 大者胜。

### 5.2 Compact Index Entry (in-memory) · 紧凑索引 entry（内存）

```
key_hash (or key ref) → PackedLoc(u64) + generation(u32) + len(u32)   ≈ 16–24 B/object
```

`PackedLoc` packing · `PackedLoc` 打包：

```
u64:  [ group_id : 24 ][ offset_in_group_blocks : 16 ][ class_id : 8 ][ flags : 8 ][ spare : 8 ]
```

- 24-bit group_id × G=256 MiB → 4 EiB addressable per device; offsets are in `BLOCK` units, 16 bits covering a 256 MiB group.
- The index structure itself: a sharded hash table (`Mutex<HashMap>` is sufficient to start; evolve under real hotspots). Full key bytes for variable-length keys are read from beside the record header only when an exact comparison is needed, or stored in an off-index arena for small keys — the index body stays fixed-size and compact.
- Capacity math: a 30 TiB disk with 512 KiB average objects → 60 M entries ≈ 1.5 GiB memory, acceptable; spilling the index to disk (on-disk hash / partitioned index) for massive small-KV workloads is future work.

---

- group_id 24 bit × G=256 MiB → 单盘可寻址 4 EiB；offset 以 `BLOCK` 计，16 bit 覆盖 256 MiB group。
- 索引结构本体：分 shard 的 hash 表（`Mutex<HashMap>` 起步即可，热点再演进）。变长 key 的全量字节仅在需要精确比对时从记录头旁读出，或对小 key 存于索引堆外 arena —— 索引主体保持定长紧凑。
- 容量账：30 TiB 盘、平均 512 KiB 对象 → 6 千万 entry ≈ 1.5 GiB 内存，可接受；海量小 KV 场景的索引下盘（on-disk hash / partitioned index）列为 future work。

### 5.3 Meta Persistence: Two Profiles · Meta 持久化：两种 profile

| | cache profile | durable profile (chunkserver) |
|---|---|---|
| Index 索引 | in-memory only 纯内存 | in-memory (journal is authoritative) 纯内存（权威在 journal） |
| Persistence 持久化 | periodic A/B snapshot (default 30 s, only when dirty, background thread — a synchronous full-table scan would cause periodic latency stalls) 周期 A/B snapshot（默认 30 s，仅 dirty 时，后台线程——同步全表扫描会造成周期性延迟卡顿） | **meta journal (WAL)**: a compact record appended per alloc/commit/dealloc, group-commit fsync; periodic checkpoint truncation **meta journal（WAL）**：每次 alloc/commit/dealloc 追加紧凑记录，group commit 合并 fsync；周期 checkpoint 截断 |
| Crash loss 崩溃损失 | recent write window lost (optionally recovered via record headers + ToC rescan) 丢最近窗口的写（记录头 + ToC 可选回扫补齐） | zero loss (journal replay + data-region seqno arbitration) 零丢失（journal 重放 + 数据区 seqno 仲裁） |
| Dependencies 依赖 | none 无 | none — no embedded LSM KV store: its write amplification and dependency weight aren't worth it; meta records are fixed-size and compact, a self-managed journal suffices 无（不引入内嵌 LSM KV 库：写放大与依赖重量不值得，meta 记录定长紧凑，自管 journal 足够） |

Journal records ≈ 32 B per operation, written to a ring area inside the meta region; a checkpoint = index snapshot + group table. Both profiles share the same data-region format and differ only in the meta channel — the same data can start life as a cache and later be upgraded to durable.

journal 记录 ≈ 32 B/次操作，写入 meta region 内的环形区域，checkpoint = 索引快照 + group table。两 profile 共用同一套数据区格式，仅 meta 通道不同 —— 同一份数据可以先当 cache 用、后升级 durable。

## 6. Write & Read Paths · 写路径与读路径

**PUT (COW)**:

1. Route: inline / log / slab class (§4.5);
2. `alloc` a new location (never overwrite the old one);
3. Assemble header+key+value in a registered fixed buffer, **one contiguous O_DIRECT write**;
4. durable: append a journal record (uncommitted); when replication semantics are needed, stop here and wait for the upper layer (§10);
5. commit: index insert-and-publish completes atomically ("insert into index + flip slot to Live" inside the same shard lock; concurrent duplicate PUTs — one wins, one retires);
6. Old version (for overwrites): after the index pointer switches, the old slot enters the retirement flow (§7.1).

**PUT（COW）**：

1. 路由：inline / log / slab class（§4.5）；
2. `alloc` 取新位置（绝不覆写旧位置）；
3. 在 registered fixed buffer 中组装 header+key+value，**单次连续 O_DIRECT 写**；
4. durable：追加 journal 记录（uncommitted）；需要复制语义时停在此处等上层（§10）；
5. commit：索引 insert-and-publish 原子完成（同一 shard 锁内完成"插入索引 + slot 置 Live"，并发重复 PUT 一胜一退）；
6. 旧版本（若为覆盖写）：索引指针切换后，旧 slot 进入退役流程（§7.1）。

**GET**:

1. Index lookup yields `PackedLoc + generation + len`; `pin_if_live` (generation checked, ABA-proof);
2. Compute the aligned read window: `[align_down(off, IO_ALIGN), align_up(off+len, IO_ALIGN))` — **single segment, single iovec**, `ReadFixed` into a fixed buffer;
3. Verify key_hash / CRC → return; mismatch → treat as miss and purge the index entry;
4. unpin.

**GET**：

1. 索引查得 `PackedLoc + generation + len`；`pin_if_live`（校验 generation，防 ABA）；
2. 计算对齐读窗口：`[align_down(off, IO_ALIGN), align_up(off+len, IO_ALIGN))`，**单 segment、单 iovec**，`ReadFixed` 入 fixed buffer；
3. 校验 key_hash / CRC → 返回；失配 → 按 miss 处理并清除索引项；
4. unpin。

**Partial reads (chunkserver `read(chunk, offset, len)`)**: same as GET, with the window being the aligned expansion of the sub-range inside the record; fragment CRCs allow verifying only the covered 64 KiB fragments.

**部分读（chunkserver `read(chunk, offset, len)`）**：同 GET，窗口取记录内子范围的对齐扩展；fragment CRC 支持只校验覆盖的 64 KiB 片段。

## 7. Reclamation & Lifecycle · 回收与生命周期

### 7.1 Slot-Level Reclamation (no movement) · slot 级回收（无搬迁）

The total order for retiring a slot · 退役一个 slot 的全序：

```
index removal (or pointer already switched) → state=Evicting → wait pin_count==0
→ generation+1 → clear bitmap bit → reusable
索引移除(或指针已切换) → state=Evicting → 等 pin_count==0 → generation+1 → 清 bitmap bit → 可复用
```

- **A pin is the only credential for in-flight references**: reads — and future RDMA direct reads — must hold a pin; eviction/deletion paths **refuse** to select pinned slots (downgraded to Retired for deferred handling).
- **Quarantine**: timed-out I/O (especially operations where the kernel or RDMA may still touch the buffer or disk range) must not have its space reused merely because it was "logically abandoned" — the slot enters quarantine until the operation is fenced (the io_uring completion arrives, or the QP/endpoint generation retires).
- Generation makes any late stale reference fail-safe: a generation mismatch is simply rejected.

---

- **pin 是唯一的在途引用凭证**：读、以及未来的 RDMA 直读都必须持 pin；淘汰/删除路径**拒绝**选中 pinned slot（降级为 Retired 延迟处理）。
- **quarantine**：超时的 I/O（尤其内核或 RDMA 仍可能触碰缓冲区或盘区的操作）不能仅凭"逻辑上放弃"就复用空间——slot 进入 quarantine，直到对应的操作 fence（io_uring completion 到达、或 QP/endpoint 代际退役）才真正释放。
- generation 使任何迟到的旧引用 fail-safe：generation 不匹配即拒绝。

### 7.2 Whole-Group Reclamation (cache eviction interface) · 整 group 回收（cache 淘汰接口）

Exposed by the engine, driven by the policy layer · 引擎暴露、策略层驱动：

```rust
trait GroupEvictionHook {
    /// The policy layer picks the victim group (the engine provides per-group
    /// stats: live count, bytes, age, class).
    /// 策略层选择 victim group（引擎提供每 group 的统计：live 数、字节、age、class）。
    fn pick_victim(&self, stats: &[GroupStat]) -> GroupId;
    /// Engine callback: enumerate surviving records in the group; the policy
    /// layer removes each from the index (or rewrites a few survivors).
    /// 引擎回调：枚举该 group 存活记录，策略层逐条从索引摘除（或挑选少量幸存者重写）。
    fn on_retire(&self, entries: impl Iterator<Item = EntryRef>);
}
```

Flow: `SEALED → retire_group → bulk index removal → QUARANTINE (wait for all pins to drain + fence) → FREE`.
One eviction reclaims 256 MiB of contiguous space with zero data movement, zero punch-hole, zero filesystem interaction. SIEVE/S3-FIFO and friends live entirely above: they may operate per-object (slab slot granularity) or per-group (log group granularity); the engine is oblivious.

流程：`SEALED → retire_group → 索引批量摘除 → QUARANTINE（等全部 pin 清零 + fence）→ FREE`。
一次淘汰回收 256 MiB 连续空间，零数据搬迁、零 punch-hole、零文件系统交互。SIEVE/S3-FIFO 等策略完全活在上层：既可以做 per-object（slab slot 粒度），也可以做 per-group（log group 粒度），引擎不感知。

### 7.3 Optional Compaction (durable only, off by default) · 可选 compaction（仅 durable、默认关闭）

Slab slot reuse means durable scenarios normally need no compaction. Only when the class distribution drifts long-term and leaves a class with many sparse groups does a background task rewrite surviving records of sparse groups into new groups at lowest priority (= the normal PUT path + index CAS switch + old-slot retirement), throttled by reserve watermarks and byte budgets. This is the only data-movement point, and it can be disabled entirely.

slab 的 slot 复用使 durable 场景通常不需要 compaction；仅当 class 负载分布长期漂移导致某 class 大量稀疏 group 时，后台以最低优先级把稀疏 group 的存活记录重写到新 group（= 正常 PUT 路径 + 索引 CAS 切换 + 旧 slot 退役），以预留水位与字节预算节流。这是唯一的数据搬迁点，且可整体禁用。

## 8. I/O Execution Layer · I/O 执行层

- **io_uring + O_DIRECT, one independent ring per worker thread**; no SQPOLL (a kernel poll thread per ring conflicts with the core-pinning budget).
- **Registered fixed buffers (`ReadFixed`/`WriteFixed`) are mandatory**: under O_DIRECT, every unregistered I/O pins/unpins its user pages on the submitting thread; pre-registered buffers pay that cost once at startup.
- **Buffer arena**: allocated once at startup, with 2 MiB / 1 GiB hugepage support (controls TLB misses; boot-reserve plus headroom prechecks prevent SIGBUS), NUMA-bound to the device's node (cross-NUMA device access caps throughput well before submission cost does).
- **iovec is always 1**: the layout guarantees every object is contiguous; write-combining batches via memcpy inside the arena rather than accumulating iovecs — large iovec arrays punt requests to the `iou-wrk` kernel thread pool and destroy latency.
- **Granularity decoupling**: allocation granularity (class/slot) is unrelated to request size. The I/O layer splits/merges by configured windows (default: reads ≤ 128 KiB per request, larger transfers split into multiple contiguous requests submitted concurrently; write flush window 512 KiB), naturally fitting 4/16/64/128 KiB workloads and NVMe MDTS.
- **Backpressure**: per-disk separate read/write in-flight byte windows (CAS reservation); read window > write window (read latency is the SLO, write is not — true for caches; durable deployments may configure them equal).
- Future work: `IORING_SETUP_IOPOLL` / NVMe passthrough; RDMA landing directly into the arena without CPU copies.

---

- **io_uring + O_DIRECT，每 worker 线程独立 ring**，不用 SQPOLL（内核 poll 线程与核绑定预算冲突）。
- **registered fixed buffers（`ReadFixed`/`WriteFixed`）为强制项**：O_DIRECT 下未注册缓冲的每次 I/O 都要在提交线程上对用户页做 pin/unpin，预注册缓冲把该开销一次性付清。
- **buffer arena**：启动期一次性分配，2 MiB / 1 GiB hugepage 支持（控制 TLB miss；boot-reserve + 余量预检防 SIGBUS），NUMA 绑定到设备所在 node（跨 NUMA 访问设备会先于提交开销成为吞吐瓶颈）。
- **iovec 恒为 1**：布局保证任意对象连续；write-combine 在 arena 内 memcpy 攒批而不是攒 iovec —— 大 iovec 数组会把请求推给 `iou-wrk` 内核线程池，破坏延迟。
- **粒度解耦**：分配粒度（class/slot）与请求大小无关。I/O 层按配置窗口（默认读 ≤128 KiB 一请求，超过则顺序切分为多个连续请求并发提交；写 flush 窗口 512 KiB）拆分/合并，天然适配 4/16/64/128 KiB 负载与 NVMe MDTS。
- **背压**：per-disk 读/写分离的 in-flight byte window（CAS 预留），读窗 > 写窗（读延迟是 SLO，写不是——cache 场景成立；durable 场景可配平）。
- future work：`IORING_SETUP_IOPOLL` / NVMe passthrough、写不经 CPU 的 RDMA 直达 arena。

## 9. Crash Recovery & Data Integrity · 崩溃恢复与数据完整性

Recovery inputs in decreasing order of authority: meta (journal or snapshot) → group table hint → group ToC → self-describing record headers.

恢复输入按权威度递减：meta（journal 或 snapshot）→ group table hint → group ToC → 记录自描述头。

- **durable**: replaying checkpoint + journal reconstructs the complete index and bitmaps; the data region is used only for verification (optional dirty-group check at startup). COW plus the "data first, meta commit second" ordering guarantees crashes only produce **uncommitted orphan records** (space temporarily leaked, naturally recovered at group reclamation — leak space rather than lose data); "meta pointing at bad data" can never occur.
- **cache**: rebuild the index from the newest valid A/B snapshot; for OPEN groups written after the snapshot, scan record headers sequentially (log groups stop at the first invalid magic/CRC; slab groups scan by ToC/bitmap hints), with `seqno` arbitrating duplicate keys. Losing the most recent write window = a few cache misses; acceptable.
- **scrub**: a slow background whole-disk patrol using fragment CRCs; bad records are dropped as misses (cache) or reported for repair (durable — the upper layer restores from replicas).
- All meta structures (superblock, group table, ToC, journal, snapshot) carry magic + CRC + epoch/seqno; corruption at any layer degrades to reconstruction from the next.

---

- **durable**：重放 checkpoint + journal 即得完整索引与 bitmap；数据区仅用于校验（启动时可选 dirty-group 校验）。COW + 「先数据后 meta commit」的顺序保证：崩溃只会产生**未提交的孤儿记录**（空间暂漏，group 回收时自然收回——宁漏空间不丢数据），绝不产生"meta 指向坏数据"。
- **cache**：读最新有效 A/B snapshot 重建索引；对 snapshot 之后写入的 OPEN group，顺序扫描记录头（log group 扫到首个无效 magic/CRC 截止；slab group 按 ToC/位图 hint 扫描），`seqno` 仲裁重复 key。丢最近窗口写入 = 若干 cache miss，可接受。
- **scrub**：后台低速全盘按 fragment CRC 巡检，坏记录按 miss 摘除（cache）或上报修复（durable，交上层从副本恢复）。
- 所有元结构（superblock、group table、ToC、journal、snapshot）自带 magic + CRC + epoch/seqno，任何一层损坏都可降级到下一层重建。

## 10. Chunkserver Extension Points · 作为 chunkserver 的扩展点

The engine provides primitives; replication/EC protocols stay above · 引擎提供原语，复制/EC 协议留在上层：

1. **Two-phase commit**: `put → persist (journal records uncommitted, data already on disk) → commit / abort`. A chain-replication head can forward after persist and commit only after checksum verification with its successor; abort merely retires the slot, with no side effects.

   **两阶段提交**：`put → persist（journal 记 uncommitted，数据已落盘）→ commit / abort`。链式复制的 head 可以 persist 后转发、与 successor 校验 checksum 后再 commit；abort 仅退役 slot，无副作用。

2. **Version semantics**: record headers and meta carry `version` (chunk_ver/commit_ver monotonicity rules defined by the upper layer), supporting CAS-style conditional updates (expected/desired tag) and idempotent replay (`last_request_id` can occupy meta-entry extension bits).

   **version 语义**：记录头与 meta 携带 `version`（chunk_ver/commit_ver 由上层定义单调规则），支持 CAS 式条件更新（expected/desired tag）与幂等重放（`last_request_id` 可加入 meta entry 扩展位）。

3. **In-chunk append**: v2 supports safe-append within the slot's remaining capacity (direct appends never crossing committed bytes + incremental fragment-CRC combine); append-heavy workloads can register exact classes with growth slack.

   **chunk 内追加**：v2 支持 slot 剩余容量内的 safe-append（direct append 不越过已提交字节 + fragment CRC 增量 combine），append 密集负载可注册带 growth 富余的 exact class。

4. **EC/replication do not affect this layer**: after EC striping, each shard is an ordinary fixed-length object (→ exact class, zero internal fragmentation); replica diffing uses the seqno/version of §9.

   **EC/replication 不影响本层**：EC 条带化后每个 shard 就是一个普通定长对象（→ exact class，内部碎片 0）；副本间同步用 §9 的 seqno/version 做差异比对。

5. **Multi-device**: one engine instance per device with clean failure domains; the upper layer composes by chain/placement.

   **多盘**：一盘一 engine 实例，故障域清晰；上层按 chain/placement 组合。

## 11. Default Parameters · 默认参数

| Parameter 参数 | Default 默认值 | Description | 说明 |
|---|---|---|---|
| `BLOCK` | 4 KiB | device logical block / alignment unit | 设备逻辑块 / 对齐单位 |
| `GROUP_SIZE` (G) | 256 MiB | fixed size, tunable 64 MiB–1 GiB | 定长，64 MiB–1 GiB 可调 |
| class spacing class 间距 | 4 per doubling for 4 KiB–4 MiB; 2 per doubling for 4–64 MiB | ~48 classes; exact-class registration supported | ≈48 class；支持 exact class 注册 |
| max object size 对象上限 | G | upper layer chunks larger objects | 上层切 chunk |
| inline threshold inline 阈值 | 512 B | stored in meta | 入 meta |
| small→log threshold 阈值 | 64 KiB | cache-profile routing | cache profile 路由 |
| record header 记录头 | 64 B | self-describing | 自描述 |
| fragment CRC | CRC32C per 64 KiB | large objects only | 大对象启用 |
| index entry 索引 entry | 16–24 B | PackedLoc + gen + len | 同左 |
| journal record | ~32 B/op, group commit | durable profile | durable profile |
| snapshot interval 周期 | 30 s | only when dirty, background thread | 仅 dirty、后台线程 |
| write flush window 写窗口 | 512 KiB | log write-combine | log write-combine |
| read split window 读切分窗口 | 128 KiB | configurable 4/16/64/128 KiB | 可配 4/16/64/128 KiB |
| FREE group reserve 预备 | min=2, max=4 per active class | background replenished | 后台补齐 |
| in-flight window | read 24 MiB / write 12 MiB per disk | empirical starting point, to be tuned | 经验起点值，待调优 |

## 12. Open Questions · 未决问题

1. **Index spill-to-disk**: with massive small KVs (billions), the in-memory index becomes the ceiling; the trade-off between an on-disk hash and a partitioned index is undecided.

   **索引下盘**：海量小 KV（十亿级）时内存索引成为上限；on-disk hash 或 partitioned index 的取舍待定。

2. **Log-group packed vs aligned**: record starts aligned to `BLOCK` (reads always single-segment aligned) vs fully packed (zero alignment loss, reads tolerate unaligned windows) is per-class configurable; the default awaits benchmarks.

   **log group 的紧凑 vs 对齐**：记录起点 `BLOCK` 对齐（读恒单段对齐）与完全紧凑（零对齐损耗、读需容忍非对齐窗口）按 class 可配，默认值待 bench。

3. **Final choice of classes per doubling**: the 16% vs 29% internal-fragmentation bound × OPEN-group footprint trade-off needs validation against real size distributions.

   **每倍频级数的最终取值**：16% vs 29% 内部碎片上界 × OPEN group 占用的权衡，需真实 size 分布验证。

4. **ZNS / FDP**: the fixed-size-group + sequential-write model is naturally affine to ZNS zones / FDP placement; whether to reserve for it in the v1 Device-layer abstraction.

   **ZNS / FDP**：定长 group + 顺序写的模型与 ZNS zone / FDP placement 天然亲和，是否在 v1 抽象 Device 层接口时预留。

5. **Meta journal placement**: same device as data (simple) vs an optional external small device; affects write-path tail latency.

   **meta journal 的盘内放置**：与数据同盘（简单）vs 可选外置小盘，影响写路径尾延迟。

6. **Where full key bytes live**: off-index arena vs hash-only with read-time comparison (false-positive probability and security trade-off; is a 128-bit salted hash sufficient).

   **key 全量字节的存放**：索引堆外 arena vs 仅存 hash + 读时比对（误判概率与安全性权衡，128-bit salted hash 是否足够）。
