# OrioleDB-on-Neon Log-is-Data 架构总纲

> **版本**：v2.1 (2026-04-18, commit-barrier 修正)
> **状态**：实施中，M1 开始
> **取代**：`docs/PAGE_LEVEL_WAL_PLAN.md`（归档于 `docs/archive/`）
> **相关**：`docs/ENTERPRISE_ROADMAP.md`（落地里程碑）

本文档是 OrioleDB 在 Neon Serverless 下达到**企业级可商用 Log-is-Data** 的 single source of truth。过时的阶段性 plan 一律归档。

---

## 1. North Star（核心不变式）

> **Commit 返回前，这次 commit 可见的关键状态已持久化到 SafeKeeper（WAL）。**
> **其他非关键状态可以 lazy 异步通过 page flush 到 PageServer，不占用额外 WAL 带宽。**

这和 PG heap 的实际语义一致——PG heap 不是"每次 buffer update 都 WAL"，而是"commit 时 WAL flush 保证 durable，buffer flush 是异步 cache 活动"。

**关键 vs 非关键** 的界定：
- **关键**（commit 必须确保已入 WAL）：
  - 本 commit 引入的 data page 变更（已有 page-level delta）
  - 本 commit 写入的 undo 条目（tuphdr.undoLocation 指向的内容）
  - 本 commit 分配的 CSN（oxid→CSN 映射）
  - 本 commit 引起的 metadata 变化，例如 root split 后的 rootDownlink
- **非关键**（可 lazy flush 异步到 PageServer）：
  - 非本 commit 涉及的 buffer eviction
  - 不影响可见性判断的 metadata 更新（checkpoint stats / leafPagesNum 等）
  - 性能 cache / 统计信息

**推论**：
- 不需要每个 state mutation 都 emit block-keyed WAL（那是过度设计）
- 不需要消灭 Plan B lazy flush 机制本身
- 只需要保证 **commit 时本 txn 涉及的关键 block 已经 flush 到 WAL**
- 本地文件仍是异步 cache，崩溃可丢

违反这条的路径（= 当前 6.6.4c-3 的根源）：commit 路径**不触发** xidmap/undo buffer 的 flush，依赖后续 eviction / checkpoint。crash 窗口正好落在 commit 与 lazy flush 之间 → commit-visible 状态在 SafeKeeper 缺失 → post-restart 数据丢失。

---

## 2. 架构三层（目标形态）

```
┌──────────────────────────────────────────────────────────────────┐
│  Layer 1 — Commit-Barrier WAL                                    │
│  commit path: flush 本 txn 涉及的关键 block 到 WAL               │
│    data pages:  已有（row-level CONTAINER + page-level delta）   │
│    undo/xidmap: 复用 Plan B FPI 机制，但从 lazy 改为 commit-eager │
│    metadata:    关键变化（rootDownlink 等）commit 时一起 flush   │
│  非关键 block 继续 lazy flush（eviction / checkpoint）            │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│  Layer 2 — COW Checkpoint = Delta Chain Re-Base                  │
│  checkpoint 不"全量 FPI"，仅在两种情况 emit FPI:                  │
│    (a) COW relocate 产生新 extent                                 │
│    (b) delta chain 过长触发 compaction                            │
│  map/control 的常规更新已在 Layer 1 commit-barrier 覆盖          │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│  Layer 3 — Compute Recovery Minimal                               │
│  restart = basebackup + PageServer reads                          │
│  WAL replay 仅重建 in-memory shmem (XID/savepoint/tree descr)     │
│  数据 / undo / xidmap / metadata 不 replay                       │
└──────────────────────────────────────────────────────────────────┘
```

**目前**：Layer 1 约 50%（data pages 有，但 undo/xidmap 仍 lazy flush）。Layer 2 未实现。Layer 3 未实现。

---

## 3. OrioleDB 持久状态 & commit-barrier 覆盖清单

| # | 状态 | 当前 | 要做什么 | 归属 |
|---|---|---|---|---|
| 1 | B-tree data pages (user/sys trees, DML) | ✅ page-level delta WAL + CONTAINER | 无改动，已达标 | — |
| 2 | Data page structural (SPLIT/MERGE/COMPACT) | ✅ FPI via page_wal | 补 `LEAF_LOCK` / `PAGE_INIT` / `ROOT_SPLIT` emit | M1.1 |
| 3 | Undo log content (tuphdr.undoLocation 指向) | ❌ 仅 lazy flush via Plan B | **commit path 触发**本 txn 涉及的 undo block flush | M1.2 |
| 4 | Xidmap (oxid→CSN commit 映射) | ❌ 仅 lazy flush via Plan B | **commit path 触发**本 oxid 的 xidmap block flush | M1.3 |
| 5 | Root downlink change (root split 后) | ❌ 仅下次 checkpoint | root split 完成时 trigger map file flush | M1.4 |
| 6 | Other map metadata (leafPagesNum / numFreeBlocks / ctid) | ❌ 仅 checkpoint | **Lazy 即可**（不影响可见性） | — |
| 7 | Control file | ❌ 仅 checkpoint | **Lazy 即可**（仅 checkpoint state，不影响 commit 可见性） | — |
| 8 | xidFile (CSN snapshot file) | ❌ 仅 checkpoint | Lazy（duplication of xidmap，post M1.3 可评估去掉） | — |
| 9 | Sys trees (O_TABLES / SHARED_ROOT_INFO 等) | ✅ 走 B-tree WAL | 验证 commit path 也触发涉及 block flush | M1.5 |
| 10 | Per-page state (CSN / pageChangeCount) | ✅ delta redo 隐含重建 | 验证 | M1.5 |

**关键洞察**：列 6-8 是 **Lazy OK** 的——它们是 checkpoint 统计或 metadata cache，不影响单次 commit 的可见性。后续 eviction / checkpoint 异步带到 PageServer 没问题。**不要**为它们加 WAL。

列 3-5 才是 commit-visible 的硬关键，必须让 commit barrier 覆盖。

---

## 4. Milestone 实施计划

### M1 — Commit-Barrier 闭合（阻塞 v0.2.0-beta.1）

#### M1.1 — Data page 死 WAL 类型激活
- 补 `orioledb_page_wal_leaf_lock` emit（`o_btree_mark_tuple_locked` 等）
- 补 `orioledb_page_wal_page_init` emit（独立 page 初始化，非 split context）
- 补 `orioledb_page_wal_root_split` emit（3-block FPI，不复用 SPLIT）
- **不是架构关键**，但消除 dead code 以免未来误用
- **工期**：2-3 天

#### M1.2 — Commit path 触发 undo block flush
- 入口：`transam/undo.c::report_commit_oxid`（或等价 commit finalize 点）
- 改造：在 commit barrier，identify 本 txn 写入的 undo 范围 → `o_buffers_flush(undo_desc, range)` → 触发 Plan B WAL emit
- **不新增 record type**，复用现有 `ORIOLEDB_XLOG_PAGE_IMAGE` FPI 机制
- **不变式**：commit 返回后，`undoLocation` 指向的所有 undo block 已 emit 到 SafeKeeper
- **工期**：4-5 天（audit commit path + 加 flush + 测试）

#### M1.3 — Commit path 触发 xidmap block flush
- 入口：`transam/oxid.c::o_xidmap_set_csn`（commit 分配 CSN 时）
- 改造：写入 xidmap buffer 后，**同步 flush 该 block** 到 WAL
- **不新增 record type**，复用现有 FPI
- **不变式**：commit 返回后，oxid→CSN 映射已在 SafeKeeper
- **工期**：2-3 天
- **优先级**：第一项实施——直接验证 6.6.4c-3 count=0 假设

#### M1.4 — Root split 触发 map file flush
- Root split 完成后 `checkpoint_map_write_header`（或等价）立即触发
- 不是"每次 metadata 变都 flush"，只是 root downlink 变化这一个特例
- **工期**：2 天

#### M1.5 — 验证 sys tree 和 per-page state 的 commit 覆盖
- 写 unit test + 小 E2E
- 如果有漏，补 targeted fix
- **工期**：2-3 天

#### M1 验收
- Phase 6.6.4 crash E2E `count == ROWS`（6.6.4c-3 解）
- `skip_unmodified_trees=false` GUC 注入**依然保留**但不依赖（作为 belt-and-suspenders）
- Commit latency 增 ≤ 10%（benchmark in M5）

---

### M2 — Checkpoint 瘦身 & 清理 Plan B 特殊路径

#### M2.1 — 移除 `checkpoint_is_shutdown` carve-out
- `o_buffers.c:233` / `checkpoint.c:2940` / `control.c:219` 三处
- M1.2 + M1.3 后，shutdown 前 buffer 已被 commit barrier flush 过，carve-out 不再必要

#### M2.2 — Checkpoint 不再强制全量 map FPI
- 前置：M1.4
- Checkpoint 只处理真正 dirty 的 metadata（不是"无条件 emit 所有 tree"）

#### M2.3 — 重新评估 xidFile 去留
- 如果 M1.3 后 xidmap block 已可信，xidFile 可能冗余 → 考虑删除 or 简化

---

### M3 — Recovery 瘦身

#### M3.1 — 删除 `skip_unmodified_trees=false` 注入
- `compute_tools/src/compute.rs` 删除相关行
- 前置：M1 全部 + 若干 E2E 连续绿
- 回归 → 回查 M1/M2 漏，**不重加**

#### M3.2 — 窄化 rmid=129 replay
- `recovery.c::replay_container` 按 `rec_type` 分流：
  - 跳过：数据类 CONTAINER（PageServer 已有）
  - 保留：shmem-reconstruction 类（XID / JOINT_COMMIT / RELATION / SAVEPOINT / SWITCH_LOGICAL_XID）

#### M3.3 — 评估 `orioledb_recovery.signal` 语义收窄

---

### M4 — Branching / PITR / Physical Replication 验证

M1 + M2 + M3 完成后，这三项应该自动可用。本 Milestone 纯验证：
- `test_e2e_branching.sh` 在非 checkpoint 边界的 LSN 分支
- `test_e2e_pitr.sh` 任意 LSN PITR
- `test_e2e_physrepl.sh` OrioleDB primary/replica 收敛

失败 → 回查 M1/M2 漏，**不在此层补丁**。

---

### M5 — 质量 & 性能

- **M5.1**：每个 WAL record type round-trip unit test
- **M5.2**：E2E 矩阵（scripts/ 下 6 个）入 CI gate
- **M5.3**：OrioleDB in-tree 43 SQL tests 在 Neon stateless 模式
- **M5.4**：Benchmark — commit latency / WAL volume vs PG heap

**M5 硬性能 gate**：
| 指标 | PG heap baseline | 阈值 |
|---|---|---|
| Single-row commit latency | x | ≤ x + 15% |
| 10K-row txn commit | x | ≤ x + 25% |
| WAL bytes/sec | x | ≤ x + 30% (因为 commit-barrier 只 flush 必要 block) |
| Restart time (10GB) | x | ≤ 2x |

---

### M6 — 交付

- **M6.1**：Doc 诚实化（本 doc + `ORIOLEDB_SERVERLESS.md`）
- **M6.2**：可选 OrioleDB upstream PR（Layer 1 作为 Serverless hook）
- **M6.3**：`orioledboutput` logical decoding plugin（独立）

---

## 5. 明确拒绝的方案

| 方案 | 拒绝理由 |
|---|---|
| Every mutation emit block-keyed eager WAL | 过度设计，WAL 量暴涨；commit-barrier 精准够用 |
| 让 PageServer 理解 CONTAINER WAL | 跨架构边界；不必要，M1 后 PageServer 从 Plan B FPI 即可还原 |
| 保留 `skip_unmodified_trees=false` 作长期 | 冷启动 O(total data) 违反 Serverless |
| 新加 shim 把 OrioleDB 映射 PG heap | 失去 IOT + CSN 优势 |
| 双路径保险（commit-barrier + lazy 并存） | 双 bug 面 + 维护×2 |

---

## 6. 不变式（每 commit 必须不违反）

1. **commit-durable**：commit 返回前，本 txn 涉及的关键 block 已在 SafeKeeper
2. **lazy-allowed**：非关键 buffer state 可异步 page flush，不占 WAL
3. **single-source-of-truth**：每个状态有唯一权威持久化路径
4. **no-replay-on-read**：backend SELECT 不触发 WAL replay
5. **LSN-precise**：PageServer 通过 (key, LSN) 查询返回任意 LSN 的 page 状态

---

## 7. 发布里程碑映射

| Tag | 包含 | 验收 |
|---|---|---|
| `v0.1.0-alpha.6` | M1.3 (xidmap commit-flush) 单项 | 6.6.4 E2E count 是否回 |
| `v0.1.0-alpha.7` | M1 全部 | 6.6.4 全绿 |
| `v0.2.0-beta.1` | M2 + M3.1 + M3.2 | skip_unmodified_trees=false 已删除 |
| `v0.3.0-beta.1` | M4 全绿 | branch / PITR / physrepl 验证 |
| `v1.0.0-rc.1` | M5 性能 gate + 测试矩阵 ≥ 80% | 硬红线不破 |
| `v1.0.0` | M6.1 docs | 可选 M6.2 upstream |

---

## 8. 为什么这一版优雅

1. **与 PG heap 一致**：commit-flush-to-SafeKeeper + lazy-page-flush，是 PG 原生模式
2. **不加不必要 WAL**：只 commit-barrier，不 every-mutation
3. **复用现有机制**：Plan B FPI 路径不动，改调用时机即可
4. **无架构补丁**：`skip_unmodified_trees=false` 必删
5. **Recovery 最简**：Layer 3 让 compute restart 干净
6. **Upstream-friendly**：Layer 1 改动局部在 commit path，可作 hook 提交

**核心洞察**：Log-is-Data 的要义是 **"commit durability through SafeKeeper"**，不是"每个 mutation 都 WAL"。当前 6.6.4c-3 的修法是让 commit barrier 正确，而不是大改 WAL 体积。
