# 战区区块状态协议 vNext 设计方向

状态：讨论稿，非当前协议合同。

本文档描述 TeamViewRelay 网络协议在 `0.8.0` 之后对 battle chunk 状态语义的一种演进方向。目标是从协议层消除
全量历史被反复装入实时快照、补丁基线和 Digest 所造成的一致性、内存、CPU 与网络扩展性问题。具体协议版本和
Protobuf 字段编号应在正式评审后确定。

## 背景与当前问题

协议 `0.8.0` 以前的模型具有以下特征：

- `BattleMapObservation` 同时承担来源观察上报和服务端历史状态输入。
- `SnapshotFull.battle_chunks` 是一个扁平列表，新连接或重新同步时可能携带房间的全部历史区块。
- `Patch.battle_chunks` 只有 upsert/delete，没有明确的 battle dataset、`base_revision` 或 `target_revision`。
- `Digest.battle_chunks` 是完整区块集合的 SHA-1 字符串。若把它作为周期变化检测手段，服务端和客户端都需要扫描、
  排序或编码完整集合。
- 活跃区块、保留历史、来源元数据和按引用查询没有清晰的生命周期边界。
- 保留时间和容量是服务端本地配置，客户端无法知道历史可能在何时被淘汰。
- 协议没有分页历史同步、增量保留窗口、稳定同步 head 或数据集 revision 确认。

Backend `1.1.2` 已通过服务端缓存、revision 变更批次、值共享、补丁缓存和正确的连接投递基线修复当前故障，
但它仍需把一个本质上独立的历史数据集投影成旧协议的快照、补丁和摘要。长期方案应把这套状态机直接写入共享协议。

## 设计目标

- 历史同步成本随本次页面或变更数量增长，而不是随房间全部历史数量增长。
- 客户端能够判断自己的数据集版本，明确处理基线不匹配、增量过期、服务端重启和历史淘汰。
- 实时观察、活跃展示状态和保留历史具有独立且可解释的生命周期。
- 所有客户端最终收敛到服务端声明的确定 revision，删除和淘汰不会留下幽灵区块。
- 保留 `BattleChunkRef + BattleChunkValue` 作为跨组件的无损逻辑模型。
- 新协议可以与 `0.8.0` 客户端并存一段迁移期。

以下内容不属于协议目标：

- 不把 Backend 的 RLE、字符串驻留、`Arc` 共享或缓存容器暴露为线格式。
- 不规定服务端必须使用内存缓存、数据库或某一种索引结构。
- 不要求客户端默认下载完整历史；仅查看实时状态的客户端可以只订阅 head 或活跃状态。

## 逻辑状态模型

### 1. 来源观察

`BattleMapObservation` 只表示某个生产端在某一时刻观察到的候选区块。观察本身不是共享历史，也不直接决定客户端
应该删除哪些既有历史。服务端继续负责来源仲裁、坐标规范化和公开值生成。

### 2. 活跃状态

活跃状态是通过来源仲裁后、仍处于实时超时窗口内的区块集合，用于地图即时显示。它可以继续随轻量实时快照或补丁
传输，但其过期不等同于从历史中删除。

### 3. 保留历史数据集

每个房间拥有一个独立的 battle chunk history dataset。数据集以 `BattleChunkRef` 为稳定键，以完整的
`BattleChunkValue` 为值，并由以下二元组唯一标识一个版本：

```text
(dataset_id, revision)
```

- `dataset_id` 是不透明的数据集世代标识。服务端清空、重建或在无法延续 revision 的情况下重启时必须更换它。
- `revision` 是数据集内单调递增的无符号整数。一次原子变更批次最多递增一次；无逻辑变化的重复上报不递增。
- 服务端为一定范围的 revision 保留 upsert 和 tombstone 变更日志，以支持增量同步。

实时 `SnapshotFull` 不再携带完整保留历史。它最多携带活跃状态和当前数据集 head；历史由独立同步流程获取。

## 建议的协议概念

以下名称用于描述职责，不预先固定最终消息名或字段号。

| 概念 | 关键内容 | 作用 |
| --- | --- | --- |
| `BattleChunkDatasetCapabilities` | 支持的同步模式、digest 算法、服务端页面上限 | 在握手或 epoch 能力中协商功能 |
| `BattleChunkDatasetHead` | `dataset_id`、当前 revision、最早可增量 revision、digest、保留时间、容量、页面上限 | 低成本公告数据集状态和策略 |
| `BattleChunkSubscribeRequest` | head-only、live patches 或 history sync 偏好 | 避免向不需要历史的客户端发送大数据集 |
| `BattleChunkSyncRequest` | `dataset_id`、已知 revision、FULL/DELTA/AUTO、页面大小、可选 cursor | 发起完整或增量同步 |
| `BattleChunkSyncChunk` | 固定 base/target revision、upsert、tombstone、序号、next cursor、是否结束 | 分页返回同一个稳定目标版本 |
| `BattleChunkDatasetPatch` | 明确的 base/target revision、upsert、tombstone | 同步完成后的实时增量 |
| `BattleChunkSyncAck` | `dataset_id` 和客户端已原子接受的 revision | 让服务端明确知道该连接的投递基线 |
| `BattleChunkResyncRequired` | 当前 head 和原因 | 在世代变化、基线不匹配或增量过期时终止错误增量 |

`BattleChunkMetaRequest` 可以保留为按 `BattleChunkRef` 的稀疏查询，但响应应声明其对应的 `dataset_id` 和 revision，
避免把不同版本查询结果误认为同一个一致快照。

## 同步语义

### 初次连接与重连

1. 服务端在握手完成后发送 capabilities 和 dataset head，不主动发送完整历史。
2. 只需要实时地图的客户端订阅活跃状态；需要历史的客户端发送 AUTO sync request。
3. 若客户端的 `dataset_id` 相同且 `known_revision` 不早于 `oldest_available_delta_revision`，服务端返回 DELTA；否则返回
   FULL 或明确要求重新发起 FULL。
4. 客户端只在完整接收并原子应用目标版本后发送 ack。中途断线不得确认部分页面。

### 分页与并发更新

- 第一个 sync chunk 固定本轮 `target_revision`，后续页面都必须描述这个目标版本。
- cursor 是不透明、短期有效且绑定房间、数据集世代和目标 revision 的令牌，客户端不能解析或跨同步复用。
- 同步期间的新变化进入高于 `target_revision` 的 revision，不得改变已经开始的页面内容。
- 客户端确认 `target_revision` 后，再从该版本接收或请求后续 patch；服务端可以把连续 patch 合并到更新的 target，但
  合并后的 base 必须仍等于客户端已确认 revision。
- cursor 失效、页面缺失或 target 不一致时，客户端丢弃本轮未提交结果并重新读取 head。

### Patch 与投递基线

- 每个 patch 必须声明 `dataset_id`、`base_revision` 和 `target_revision`。
- 客户端仅在本地版本恰好等于 base 时应用 patch；否则请求重新同步，不能猜测或跳过缺口。
- 服务端以最后收到的 ack 作为连接基线，不能以“已写入 socket”推断客户端已经接受数据。
- 拥塞时允许服务端丢弃未确认的中间广播并从已确认 revision 重建合并 patch；不得无限排队历史帧。

## Digest 与规范化

revision 是变化检测的主信号，digest 只用于完整性验证，不再要求双方按固定周期重新扫描全部历史。

- head 必须同时声明 digest 算法和结果，建议首个算法使用基于 SHA-256 的版本化规范，例如
  `sha256-merkle-v1`。
- 叶节点由规范化的 `BattleChunkRef + BattleChunkValue` 生成，规范必须固定字段缺省值、字符串编码、整数编码和排序；
  `observedAt` 等非逻辑元数据不得进入摘要。
- 服务端应缓存已提交 revision 的 digest；实现可用 Merkle tree 或其他等价增量结构把更新成本控制在
  `O(changes × log N)`，协议不暴露内部树结构。
- sync 完成后客户端验证目标 head 的 digest。失败时丢弃未提交结果并请求 FULL，同时记录可观测错误。
- 协议 `0.8.0` 所需 SHA-1 摘要仅作为兼容投影保留，不作为 vNext 数据集的变化检测机制。

## 保留、容量与删除

- head 公告服务端当前实际生效的 `retention_seconds` 和 `max_entries`；这些是策略信息，不保证某条记录一定存活到上限。
- 活跃超时只改变实时活跃集合，不删除历史值。
- 显式删除、时间过期、容量淘汰和管理员清理都属于历史数据集变更，必须产生新 revision 和 tombstone。
- tombstone 至少保留到对应 revision 早于 `oldest_available_delta_revision`。客户端落后超过这个窗口后必须执行 FULL，
  不能仅接收当前 tombstone 子集。
- 达到容量上限时，淘汰顺序必须由服务器实现确定且可重复；线协议只承诺最终状态和 revision，不要求暴露内部队列。
- 数据集被整体清空或无法延续变更日志时应更换 `dataset_id`，而不是把 revision 静默重置为零。

## 兼容与迁移

1. 在共享协议仓库中以高于 `0.8.0` 的新 epoch 定稿消息、字段号、规范化向量和 breaking 检查。
2. Backend 先实现双栈状态：内部以 vNext 数据集为真值，为 `0.8.0` 连接继续生成旧
   `SnapshotFull.battle_chunks`、`Patch.battle_chunks` 和 SHA-1 Digest 投影。
3. Mod 和 Web Script 接入 capabilities、head、分页同步、ack 与 resync；未声明能力的客户端始终走旧投影。
4. 通过管理指标观察 FULL/DELTA 次数、同步页面大小、base mismatch、digest mismatch、淘汰数量和旧协议连接数。
5. 三端完成发布并经过兼容期后，再单独决定是否提高最低协议版本并删除旧投影。

迁移期间，内部压缩必须仍能无损还原公开的 `BattleChunkValue`。不能为了降低内存而让新旧协议看到不同逻辑值。

## 验收标准

- 65,536 条以上历史可以通过有界页面完成 FULL，同一 WebSocket 消息不会随全部历史无限增长。
- 在大历史上修改少量区块时，编码、摘要和网络成本随变更数增长，不再每个广播或 Digest 周期处理全部历史。
- 分页期间持续收到观察上报，客户端仍先收敛到固定 target revision，再通过 DELTA 收敛到最新 head。
- 重连时可从已确认 revision 增量恢复；服务端重启、dataset 清空或 revision 窗口过期会确定性转为 FULL。
- 时间淘汰、容量淘汰、显式删除和管理员清理均能通过 tombstone 或 FULL 使客户端删除旧值。
- 重复、乱序、缺页和 base mismatch 不会被静默接受，也不会形成无限重同步循环。
- 同一组规范化测试向量在 Backend、Mod 和 Web Script 中产生相同的 digest。
- 协议迁移期内，`0.8.0` 客户端保持当前可用行为，且兼容投影不会污染 vNext revision 状态。
