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
