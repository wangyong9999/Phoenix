# M1 推进 Session Notes

> **目的**：记录 commit-barrier 模式下 M1 多项实施的现状 / 假设 / 结论 / 风险点，
> 以便 session compact 后下一次接手能直接续。不是架构文档（那是 `LOG_IS_DATA_ARCHITECTURE.md`）。

## 当前 HEAD

最新 commit: `0864d31` "M1.2 — commit-barrier undo log flush"
(Phoenix CI run 24607584546 正在跑)

## M1 子项进度

| 子项 | 提交 | 状态 |
|---|---|---|
| M1.1 — 数据页死 WAL 类型激活 (LEAF_LOCK/PAGE_INIT/ROOT_SPLIT) | — | ❌ 未做。**非阻塞**，只补代码一致性，无功能缺失 |
| M1.2 — undo log commit-barrier flush | `0864d31` | ✅ 实施，CI 跑中 |
| M1.3 — xidmap commit-barrier flush | `158119b` | ✅ 实施，单项 CI 仍 count=0（因 undo 未覆盖）|
| M1.4 — map file header (rootDownlink) eager flush | — | ❌ 未做。依赖 M1.2+M1.3 先绿 |
| M1.5 — sys tree / per-page state 验证 | — | ❌ 未做 |
| M1.6 — 清理 Plan B lazy carve-out | — | ❌ 未做 |
| M1.7 — sys tree WAL 验证 | — | ❌ 未做 |

## 关键代码位置速查

### Commit barrier 主入口
- `pgxn/orioledb/src/transam/oxid.c::current_oxid_commit` (around line 1472)
  - 接收 CSN
  - M1.2 undo flush（新）— 调 `fsync_undo_range` per undoType
  - `set_oxid_csn` — 快路径 CAS circular xidBuffer
  - M1.3 xidmap flush（新）— `o_buffers_write` + `o_buffers_sync`
  - pg_write_barrier + cleanup

### Caller
- `pgxn/orioledb/src/transam/undo.c:2219` — 主 commit path (`finish_active_xact`)
- `pgxn/orioledb/src/transam/undo.c:2680` — 次 commit path (prepared transactions?)

两个 caller 都调 `current_oxid_commit(csn)`，所以我们的 barrier 覆盖两处。

### 关键 persistent state
```
data pages      -> page_wal.c LEAF_INSERT/UPDATE/DELETE emit (已有)
undo log        -> o_buffers lazy flush -> M1.2 改为 commit barrier
xidmap (CSN)    -> circular buffer + o_buffers lazy -> M1.3 改为 commit barrier
map file header -> checkpoint-only -> M1.4 未做
control file    -> checkpoint-only -> M1.5 未做（可能不需要立即做）
```

### Plan B FPI emit 点
`pgxn/orioledb/src/utils/o_buffers.c::write_buffer_data`
- guard: `obuffers_planb_enabled(desc) && !checkpoint_is_shutdown`
- 当 M1.2/M1.3 触发 flush 时，此函数自动 emit `REGBUF_FORCE_IMAGE` FPI

## 调试工具

### 诊断日志
- `ckpt_fill` / `ckpt_init_new` 在 checkpoint.c（已在，DEBUG1）
- 可在 `current_oxid_commit` 加 `elog(LOG, "M1.2/M1.3 committed oxid=%lu csn=%lu", oxid, csn)` 确认路径执行

### CI run download
```
gh run view <run_id> --repo wangyong9999/Phoenix --log 2>&1 \
  | grep "Run crash-mid-checkpoint" \
  | grep "PG:" | awk -F'GMT ' '{print $2}'
```

### 本地 build
```
cd pgxn/orioledb && make -j4 USE_PGXS=1 \
  PG_CONFIG=/home/alen/cc/orioledb-neon/pg_install/v17/bin/pg_config
```

## 假设 & 尚未验证

1. **M1.2 + M1.3 联合解 6.6.4c-3 count=0 假设**
   - pre-crash commit 500 rows → undo writes + CSN → M1.2 flush undo, M1.3 flush CSN → XACT record XLogFlush → SafeKeeper 有全部 state
   - post-crash new compute 读 PageServer 的 undo block 能拿到 undo item → tuphdr.undoLocation 解引用成功 → xidmap 能拿到 CSN → visibility 正常 → SELECT count=1000
   - **如果 CI 仍 count=0**：
     - 可能：data pages 的 FPI 本身没 reach PageServer（看 CI log `checkpoint_map_write_header` 和 `write_page_to_disk`）
     - 可能：rootDownlink 指向新 block，但新 block 没 FPI（需 M1.4）
     - 可能：OrioleDB 有其他 lazy flush 路径未覆盖

2. **get_cur_undo_locations 是否真返回本 txn 的 undo tip**
   - 假设 `locs.location` = 此 backend 当前 undo 写入最高 location
   - 如果实际是别的语义（例如 subxact 级别），flush range 可能错
   - **验证法**：加 elog 打印 locs.location vs meta->writtenLocation

3. **commit fast path 之外还有其他 commit path 吗？**
   - 检查过：`current_oxid_commit` 是主入口
   - 但某些 2PC / prepared transaction 可能走别的路径？需 audit

## 风险点 / 注意事项

### 性能
- fsync_undo_range 内部 evict + sync，可能 O(dirty buffer) 时间
- 大 txn 会 flush 多 buffer → 多 XLogInsert（每个 FPI ~8KB）
- 预期 commit latency 增加 1-5 ms（PG heap 通常 100-500µs，所以显著 overhead）
- **如果 benchmark 看到 >20% 回归**：
  - 优化 1: commit group (batch flush 多个 commits)
  - 优化 2: 延迟 flush 到 commit 最后，XLogFlush 前统一做

### Crit Section 约束
- `current_oxid_commit` 不在 CRIT_SECTION
- `fsync_undo_range` / `o_buffers_sync` 内部使用 LWLock，**不能**在 CRIT_SECTION 里调
- 当前实现没在 CRIT_SECTION，OK

### XLogInsertAllowed() 检查
- `write_buffer_data` 内部 guard `obuffers_planb_enabled(desc)` 检查 `XLogInsertAllowed()`
- 正常 commit 时 XLogInsertAllowed()=true，OK
- shutdown / recovery 期间返回 false → 跳过 FPI emit
- 后一种场景是 ok 的（shutdown 时不需要发新 WAL）

## 下一步决策树（Compact 后用）

**当前 CI 24607584546 结果未知**。两种 outcome：

### 情况 A: count=1000 ✅
- M1.2 + M1.3 联合解决了 6.6.4c-3
- 下一步：
  1. 删除 `skip_unmodified_trees=false` 注入 (M3.1)
  2. CI 验证没回归
  3. 发版 alpha.6
  4. 推 M1.4 (map file rootDownlink eager)

### 情况 B: count 仍 != 1000
- M1.2 + M1.3 不够
- **诊断步骤**:
  1. 下载 CI log，看 post-crash logs for (5, 16476)：
     - `evictable_tree_init_meta: (5,16476) INIT fork nblocks=?`
     - 是否有 `Page version 0` FATAL（应该没了）
     - tree traversal 到 leaf 了吗
  2. 加诊断：在 `orioledb_redo_leaf_insert` 加 elog 确认 replay 执行
  3. 可能需要 M1.4 先做（rootDownlink eager） — post-checkpoint root split 场景
  4. 可能 pre-crash 根本没 commit？检查 `before-crash: count=1000 checksum=...`——之前 CI 看到 1000 before-crash，确认 pre-crash 已 commit

### 情况 C: 新 FATAL / 其他
- 可能性: fsync_undo_range 在 commit path 有 deadlock / assert
- 看 CI stack trace

## Session 回显的关键事实

- **xidBuffer** 是 OrioleDB 进程内 ring buffer，**不**走 OBuffers。M1.3 强制走 OBuffers path 才能 FPI。
- **undoBuffersDesc** 在 undo.c 是 static，但 `fsync_undo_range` 是 public API。
- **Plan B FPI** emit 发生在 `write_buffer_data`，REGBUF_FORCE_IMAGE 标志，PageServer 把 FPI 存为 `Value::Image`。
- **`smgr_hook != NULL`** 是 Neon 模式检测的一致方式（用于所有 Neon-only 代码路径 gate）。
- **`skip_unmodified_trees=false`** 还在 compute.rs 中注入，作兜底。只要 M1 完成就可以删。

## 代码 touch points 清单 (便于 grep)

```
oxid.c::current_oxid_commit           # M1.2 + M1.3 改造点
o_buffers.c::write_buffer_data        # Plan B FPI emit
o_buffers.c::o_buffers_sync           # flush 公用 API
undo.c::fsync_undo_range              # undo flush 公用 API
compute.rs::skip_unmodified_trees     # M3.1 要删
```

## 落盘原因

session 预算接近 Compact，这些是 LOG_IS_DATA_ARCHITECTURE.md 不适合放的"过程性注释"。
CI 跑完后如 Compact 执行，下次 session 从本文件 + 架构总纲接续。
