# Phase 3 Preflight Audit

Scope: pure read-only audit of the interim signal-path architecture
to pin down what must change (and what does NOT) before Phase 3
(retire `orioledb_recovery.signal` + compute-side rmid=129 replay).

Produced 2026-04-21 as the gate before Phase 3 阶段 1 spike.

## 结论速览

- **CONTAINER retirement 是安全的**：retire 指的是消费端 (apply_btree_modify_record)，生产端保留（logical decoding 还用）。零代码生产端改动。
- **sys-tree 物化已经走 page-level WAL**：每个 Persistence sys-tree 的 mutation 都 emit LEAF_\* / SPLIT / MERGE / PAGE_IMAGE。无缺口。
- **唯一明确的 P3 前置缺口**：cold-start summary 缺 undo_meta 位置。需要扩到 B.5。
- **其它潜在缺口**：xidmap 指针、`lastCheckpointNumber` 过检查点边界的单调推进——需要阶段 1 spike 暴露。

---

## Audit A — CONTAINER 生产 / 消费路径

### 生产端（1 个 site）

| 位置 | 何时触发 |
|---|---|
| `recovery/wal.c:980` `XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_CONTAINER)` | `add_modify_wal_record_extended` 末尾；backends 在事务时间调用，`!is_recovery_process()` 断言保证 recovery 不重复发射 |

所有上层 API (`o_wal_insert/update/delete/reinsert/delete_key`、toast.c 直调)
最终都汇入这一个 XLogInsert 点。

### 消费端

| 位置 | 角色 | I4 状态 |
|---|---|---|
| `recovery/worker.c:678` `apply_btree_modify_record(&id->desc, ...)` | signal-path 的 recovery worker pool 分发 tuple-level replay | **I4 违规**（compute 上 replay）|
| `recovery/recovery.c:4305` `apply_sys_tree_modify_record` → `apply_btree_modify_record(get_sys_tree(...))` | signal-path 的 sys-tree 专用分发 | **I4 违规** |
| walingest `libs/wal_decoder/src/decoder.rs` (rmid=129 分支) | 从 stream 中提取 OrioleDBColdStartSummary delta（B.3/B.4） | ✅ 不 materialize pages，只聚合 xid/csn 边界 |
| `log_logical_wal_container` (R14 logical decoding) | 为下游 logical decoder 生产 row-level 解码流 | ✅ 不参与 page materialization |

### 判断

Retire signal-path 只删除前两个 consumer。生产端继续发射，walingest 聚合不受影响，R14 decoding 不受影响。

**风险**：signal-path 死代码保留会让 `apply_btree_modify_record` 持续占用构建体积+认知负担；建议 Phase 4 cleanup 删之。

---

## Audit B — sys-tree mutation → WAL 路径矩阵

### 结构

所有 sys-tree 写入路径汇入 `o_btree_modify` / `o_btree_autonomous_{insert,delete}` → `o_btree_normal_modify` (btree/modify.c)。内部分两股独立 WAL：

```
o_btree_normal_modify(...)
├── 页面状态转换点 →  orioledb_page_wal_{leaf_insert, leaf_delete, leaf_update}
│      (from insert.c:945,1161,1170; modify.c:879,959)
│      → XLogInsert(ORIOLEDB_RMGR_ID, LEAF_*) block-keyed FPI   ✅ Plan E
│
└── 事务-level API （可选） → o_wal_insert/update/delete/reinsert
       (from tableam/operations.c; btree/modify.c:1542/1593/1595)
       → add_modify_wal_record → XLogInsert(ORIOLEDB_RMGR_ID, CONTAINER)
       ✅ logical decoding; ✗ retire signal-path 后这部分 consumer 失业
```

SPLIT / MERGE / PAGE_IMAGE 也是 page-level，block-keyed（已验证 R9/R11）。

### 抑制条件

- `orioledb_page_wal_enabled()` = `smgr_hook != NULL && XLogInsertAllowed()` — recovery 中返回 false
- `add_modify_wal_record` 路径 `Assert(!is_recovery_process())` — recovery 中不发射

⇒ 同一 mutation 不会被 double-WAL，replay 时也不会再次递归 emit。

### sys-tree 矩阵（按 index）

| idx | 名称 | storageType | WAL 路径 |
|---|---|---|---|
| 1 | SHARED_ROOT_INFO | InMemory | 无 WAL ⇒ 每次 cold-start 重建 |
| 2 | O_TABLES | Persistence | 双路 ✓ |
| 3 | O_INDICES | Persistence | 双路 ✓ |
| 4 | OPCLASS_CACHE | Persistence | 双路 ✓ |
| 5 | ENUM_CACHE | Persistence | 双路 ✓ |
| 6 | ENUMOID_CACHE | Persistence | 双路 ✓ |
| 7 | RANGE_CACHE | Persistence | 双路 ✓ |
| 8 | CLASS_CACHE | Persistence | 双路 ✓ |
| 9 | EXTENTS_OFF_LEN | Temporary | 无 WAL ⇒ 每次 cold-start 重建 |
| 10 | EXTENTS_LEN_OFF | Temporary | 无 WAL ⇒ 每次 cold-start 重建 |
| 11-24 | 其它 syscache / map trees | 多数 Persistence | 双路 ✓ |

### 判断

- Persistence sys-tree 在 page-level stream 里完整覆盖——Phase 3 compute 通过 PageServer GetPage 直接 materialize，不需要 replay。
- Temporary / InMemory sys-tree 一直靠 process-local 重建，不被 Phase 3 影响。

---

## Audit C — 冷启动 shmem 依赖 vs summary v2

`checkpoint_shmem_init`（checkpoint/checkpoint.c:258）在 compute 冷启动时初始化 shmem。两阶段：

### 阶段 1：从 control file 恢复（已 work）

来自 `get_checkpoint_control_data` → PageServer 读 Plan E 镜像的 control file FPI：

| shmem 字段 | 源 | 状态 |
|---|---|---|
| `checkpoint_state->lastCheckpointNumber / controlReplayStartPtr / controlSysTreesStartPtr / controlToastConsistentPtr / mmapDataLength` | control file | ✓ |
| `undo_meta->{lastUsedLocation, writtenLocation, minProcRetainLocation, checkpointRetain{Start,End}Location, ...}` for each undo log type (3 个) | control file | ✓ 在 checkpoint **时**快照 |
| `xid_meta->{nextXid, runXmin, globalXmin, writtenXmin, writeInProgressXmin, checkpointRetainXmin/max, cleaned*}` | control file | ✓ 在 checkpoint **时**快照 |
| `startupCommitSeqNo` | control file | ✓ |

### 阶段 2：从 summary 追平到 end-of-log（apply_orioledb_cold_start_summary）

当前 summary wire v2（48 字节）：
- magic `OROS` / version 2
- `next_oxid`, `last_pg_xid_seen`, `last_ingested_lsn_raw`, `ingested_count`, `next_csn`

`apply_orioledb_cold_start_summary` 做的：
- `xid_meta->nextXid` / `runXmin` / `globalXmin` / `writtenXmin` 单调 bump 到 `next_oxid`
- `startupCommitSeqNo` 单调 bump 到 `next_csn`

### ⚠ 已识别的缺口

| shmem 字段 | 当前从哪里取 | Phase 3 之后怎么办 |
|---|---|---|
| `undo_meta->lastUsedLocation` （最后一次 undo 写入位置） | checkpoint 时快照；signal-path replay 时由 CONTAINER 记录推进 | **缺口：summary 没有**。下一个 undo 写入会从 checkpoint 时的老位置开始，**覆盖** checkpoint→crash 之间已写但未 clean 的 undo。需 B.5 扩展。|
| `undo_meta->minProcRetainLocation` 等 MVCC 保留水位 | 同上 | 同样缺口，需 B.5 |
| xidmap / `XidFileRec` 队列 | signal-path replay 重放 XID record | **需检查**：阶段 1 spike 看有没有具体报错，目前无法纸上判断 |

### 未知（阶段 1 spike 来暴露）

- 某些 GUC / sys_cache bootstrap 路径是否在 `!RecoveryInProgress() && !AmStartupProcess()` 的 guard 外假定"我正在 replay"——要看异常日志。
- Plan B `read_buffer_planb_fallback` 对 xidmap 的 coverage 是否完整（代码层判断 tag mapping 对所有 `planBLogId != 0` 都生效；但实战可能暴露某 log id 初始化顺序 bug）。

---

## 对 Phase 3 计划的影响

| 原计划阶段 | 当前判断 | 调整 |
|---|---|---|
| 阶段 1 spike | **可直接做**。去 signal 生成端 → 观察 CRUD/crash 输出 | 不调整 |
| 阶段 2 定向补足 | **已有确定工作项**：B.5（undo_meta + xidmap positions into summary）| 提前开卡 |
| 阶段 3 切换默认路径 | 需先关阶段 2 | 不调整 |
| 阶段 4 清理 | 删 `apply_btree_modify_record` + signal 相关 vendored PG patch | 不调整 |

## 下一步

进入 **阶段 1 spike**。在 worktree 里起一个可丢弃分支：

1. `compute_tools/src/compute.rs` — 跳过 `write_orioledb_recovery_signal` 调用（保留 basebackup + sync_safekeepers）
2. 跑 `bash scripts/test_e2e_crud.sh` 观察：
   - 最好结果：count=4000 对上 → 阶段 2 只剩打包
   - 次好结果：count 错但 PG 不 crash → 诊断具体缺口，大概率命中 B.5（undo 位置）
   - 最坏结果：某 assert PANIC → 记录 stack trace，回阶段 0 再审一轮

保持 spike 在 worktree 内，**不合并到 main**。

---

## 阶段 1 spike 结果（2026-04-21 执行）

**Patch（已回滚）：** `compute_tools/src/compute.rs:1772` 把 `if sync_lsn_present { ... }` 加 `ORIOLEDB_SPIKE_SKIP_SIGNAL` 环境变量开关。绕过 WAL copy + signal 写入 + `skip_unmodified_trees=false` GUC 推送三件事。

### 正面信号（Phase 3 核心机制证明通）

在 CRUD 测试的 restart 路径里，**lazy-load cold-start 机制成功运转**：

```
[744907] OrioleDB: control file loaded from PageServer (chkp=1)
[744907] OrioleDB: deferred control file load from PageServer, chkp_num=1, reset 24 sys trees
[744907] evictable_tree_init_meta: (1, 2) root loaded ... chkpNum=1 itemsCount=2 ...   # O_TABLES
[744907] evictable_tree_init_meta: (1, 3) root loaded ... chkpNum=1 itemsCount=4 ...   # O_INDICES
[744907] evictable_tree_init_meta: (1, 4) root loaded ... chkpNum=1 itemsCount=3 ...   # OPCLASS_CACHE
[744907] evictable_tree_init_meta: (5, 16476) root loaded ... chkpNum=1 itemsCount=35 level=1  # 用户树
```

所有 sys-tree 和用户表的 root 都从 PageServer 正确加载。`recovery_requested=0` 全程。`apply_orioledb_cold_start_summary` 自动 bump xid/csn state（B.3/B.4 管道按预期工作）。这**证实**了 Phase 3 的 lazy-load + summary apply 主路径是可行的。

### 发现的 P3 硬缺口（G1）

**G1 — SIGKILL-before-first-checkpoint 场景**：`test_e2e_crash_concurrent.sh` 第二 session 撞进：

```
[747336] checkpointable_tree_init: (1, 2) chkp_num=0 concurrent=0
[747336] evictable_tree_init_meta: (1,2) INIT fork smgrexists=1
[747336] evictable_tree_init_meta: nblocks=0 have_map=0
...
[747336] TRAP: failed Assert("o_table"), File: "src/tableam/scan.c", Line: 217
```

**根因链**：
1. 第一 session 起 PG → SIGKILL → 整个过程中**从未跑过 checkpoint**（并发 INSERT 在 CREATE TABLE 刚结束时就被 kill）。
2. PageServer 收到了所有 page-level LEAF_INSERT / PAGE_IMAGE WAL（mutation 路径每次发射），但**从未收到 checkpoint_map_write_header 发射的 map file FPI**，也**从未收到 `write_checkpoint_control` 发射的控制文件 FPI**。
3. 第二 session cold-start：basebackup 在 post-crash LSN 0/1EF52A8 展开 → `get_checkpoint_control_data` 失败（PageServer 里没有 control file FPI）→ `sys_trees_load_control_if_deferred` 不执行 → `lastCheckpointNumber` 保持 0 → 树 init 用 chkp_num=0 → map file 读到 nblocks=0 → shmem 树为空 → o_tables_get 返回 NULL → SELECT planner assert 炸。

**这是真正的 Phase 3 blocker，不是 summary 字段缺口。**

### G1 形状：三种可能 fix

| 方案 | 思路 | 代价 | 长期架构评价 |
|---|---|---|---|
| G1-a | 把 map file + control file 内容纳入 summary v3，走 B.3 管道 | summary 膨胀（每 tree 一个 rootDownlink+chkpNum，~16 字节/tree）| ✅ 最对齐 Log-is-Data：summary 成为 compute 的单一依赖 |
| G1-b | 让 mutation 时顺便把更新后的 rootDownlink 塞 WAL（每次 tree 根 COW 时发一条微型 FPI）| emit 频率大涨 | ❌ WAL 量爆炸 |
| G1-c | compute 启动时强制 basebackup 包含一个"合成 control file"，由 walingest 根据 page-level WAL 自己反推 tree 根 | PageServer 端需要半解析 rmid=129 | 中性：复杂度转移到 PageServer |

**推荐 G1-a**：walingest 已经在消化 rmid=129 流（B.3/B.4）。扩展它同时记录每个 tree 的最新 rootDownlink + 当前 chkpNum 是最自然的延伸。basebackup 时把这个"逻辑 control file"一起投递，替代从 PageServer Plan E 镜像拉 control file。

### G2（次级，可与 G1 并行修）

**G2 — CRUD restart 后 user table `SELECT count(*)=0` 但 sys-tree 完整**：
第二 session 加载了用户表 root (chkpNum=1 itemsCount=35 level=1)，但 PageServer log 零次命中用户表 blkno 的 GetPage。需要加 elog 到 `btree_smgr_read` / `read_page_from_disk` 路径精细定位，当前未解。可能 2 slot 数据文件 + PageServer 单 blkno-per-rel 语义之间有错位——独立于 G1 的侧边 bug。

### 对 Phase 3 计划的调整

| 阶段 | 原预期 | spike 后调整 |
|---|---|---|
| 阶段 1 | 探路 | ✅ 完成；主路径机制 OK，暴露 G1 + G2 |
| 阶段 2 | B.5 undo/xidmap positions | **提升：变成 B.5 summary schema 重做——把 tree roots (map file 等效) + control file 要点一并纳入** |
| 阶段 3 | 切默认 | 不变，需先关阶段 2 |
| 阶段 4 | 清理 | 不变 |

**阶段 2 具体工作（初步）**：

1. 设计 summary v3 wire 格式：固定部分（xid/csn 同 v2）+ 每 sys-tree / user-tree 一个 `TreeRootEntry { datoid, relnode, chkpNum, rootDownlink }`。user-tree 数可变。
2. walingest 侧：观察 rmid=129 里的 SMGR_CREATE + PAGE_IMAGE blocks，维护当前每 tree 的最新 rootDownlink。
3. basebackup 投递 summary v3（复用 ORIOLEDB_STATE_KEY keyspace 条目，或新增 per-tree entry）。
4. compute 侧 `apply_orioledb_cold_start_summary`：除了现有 xid bump，还要依据 summary 重建"假 control file"，或直接把 tree roots 灌进 `checkpoint_state`。

**G2 诊断先行**：不卡住阶段 2。可以并行起一个 debug elog branch 排查。

---

## 阶段 2 实施简化（B.5 revised, commit 9f1bfed）

经核实（见下节对话和 docs/B5_SUMMARY_V3_SCHEMA.md 初稿 vs 简化），**上面的 summary v3 大改动不必要**。G1 的最小闭合方案：

**改动范围**：~70 行 C，仅 OrioleDB 扩展，无 pageserver / wal_decoder / compute_tools 改动。

- `page_wal.c` 新增 `orioledb_page_wal_emit_map_header(BTreeDescr *)` helper：在 INIT fork block 0 emit 一条 `ORIOLEDB_XLOG_PAGE_IMAGE`，内容为 minimal `CheckpointFileHeader{ rootDownlink, datafileLength=1, leafPagesNum=1 }`
- `btree.c:o_btree_init` 尾部调用这个 helper（在现有 root FPI 之后）
- **无**新 info byte；**无**新 record 类型；**无** summary schema 改动；**无** walingest / compute apply 改动
- 依赖既有 `evictable_tree_init_meta` 的 Plan E fallback（checkpoint.c:5613-5690）lazy-load INIT fork

### 为什么这个够用

Root 物理位置在 Neon 模式下**不随 IUD 搬迁**——`o_btree_finish_root_split_internal` (insert.c:222) 显式保留 rootDownlink；root merge 同理。只有 checkpoint 的 COW 会搬 root，而 checkpoint 本来就发 `checkpoint_map_write_header` 覆盖 INIT fork block 0。

所以 manifest 只需要在"从无到有"的那一刻发——`o_btree_init` 是唯一缺口。

## 阶段 1-spike 重跑（B.5 + spike）结果

2026-04-21 重跑 crash_concurrent 验证：B.5 committed + compute_tools spike 重启（skip signal-path）。

- **G1 彻底闭合**：24 个 sys-tree 全部 INIT fork `smgrexists=1 nblocks=1 have_map=1`，roots 从 PageServer 成功 lazy-load。`o_tables_get` 返回 crash_concur 的 OTable 描述符（无 `Assert("o_table")`）。test 从 [5/10] 通过进入 [6/10]。
- **R10 在 spike 配合下不触发**：无 signal-path 即无 end-of-recovery checkpoint 调用，checkpoint_ix sys-tree (1,8) 的 hang 点不被触及。R10 自然闭合于 Phase 3 阶段 3 切换默认。
- **新 blocker（非 cold-start 域）**：[6/10] `SELECT count(*)` 撞 `Assert("tuplen <= sizeof(dst->fixedData)")` at `page_contents.c:605 copy_fixed_key`。

### 新 blocker 分析（G3）

- 位置：scan 遍历 leaf 页时 `copy_fixed_key` 读取 key 长度超 `OFixedKey::fixedData` 上限
- 场景：4 个并发 backend 同时 INSERT + SIGKILL 的 leaf 页内容——有可能是并发写入+PageServer 单 blkno-per-rel 语义下 last-writer-wins 导致的 partial/stale leaf 被 GetPage 返回
- **不**属于 cold-start 域（tree 找得到、load 成功了）。属于并发写入原子性/正确性域——该走 R17 类排查，非 Phase 3 架构层
- 独立于 Phase 3 继续推进，与 Phase 4 concurrent-write crash 硬化并行

## 对 Phase 3 整体计划的更新

| 阶段 | 状态 | 下一步 |
|---|---|---|
| 阶段 0 审计 | ✅ 完成 | — |
| 阶段 1 spike | ✅ 完成，路径通过 | — |
| 阶段 2 实施 | ✅ **简化后 B.5 已 commit 9f1bfed** | — |
| 阶段 3 切换默认 | ⏸ 下一步候选 | compute_tools 默认跳过 signal-path（可能仍需 fallback 为 option）|
| 阶段 4 清理 | ⏸ 阶段 3 稳定后 | 删 `apply_btree_modify_record` 调用链 |

**阶段 3 前置风险**：G3（concurrent-write crash）仍开放。**可以阶段 3 和 G3 解耦**——单线程工作负载（CRUD / crash_2pc / crash_savepoint 等）在 Phase 3 模式下应已通，先切它们；crash_concurrent 作为独立 R 项跟踪。
