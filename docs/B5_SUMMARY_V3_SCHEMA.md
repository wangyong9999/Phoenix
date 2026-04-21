# B.5 — Summary v3 + Tree Manifest Emission Design

Draft 2026-04-21. Phase 3 阶段 2 核心设计。

## 问题陈述（B.5 要解决的）

Phase 3 spike (`docs/P3_PREFLIGHT_AUDIT.md` 阶段 1 结果) 证实：lazy-load cold-start 主路径机制通——但在 **"mutation happened，尚未 checkpoint"** 的窗口内撞 crash，第二 session 找不到 tree 根。

根因：OrioleDB 的 tree 元数据（`rootDownlink`, `datafileLength`, 控制文件字段）**只在 checkpoint 时才写进 PageServer**：

| 元数据 | 当前发射点 | 发射频率 | 问题 |
|---|---|---|---|
| `CheckpointFileHeader`（INIT fork block 0，即 map file）| `checkpoint_map_write_header` → `XLogInsert PAGE_IMAGE` | 仅 non-shutdown checkpoint | 无 checkpoint 时 PageServer 上的 rootDownlink 过时 |
| Control file（合成 rel `ORIOLEDB_CONTROL_FILE_OID`）| `write_checkpoint_control` → `XLogInsert PAGE_IMAGE` | 仅 non-shutdown checkpoint | 同上 |
| Undo retention watermark | 在 control file 里 | 同上 | 同上 |

Signal-path 时代靠 replay CONTAINER 在 compute 侧重建这些；retire 之后没人补了。

## 设计目标

1. **Summary 成为 compute cold-start 的完整元数据源**——不再依赖 PageServer 上的 checkpoint-at-rest 镜像
2. **Summary 实时性由 walingest 的 rmid=129 消化保证**——每条 commit 推进 summary
3. **emit-side 最小侵入**：不在热路径（per-row mutation）加 FPI 发射
4. **为 Phase 4 cleanup 铺路**：`apply_btree_modify_record` 及其调用链成为纯死代码，可安全删除

## 架构

```
┌─ backend mutation ────────────────────────────────────────┐
│ o_btree_normal_modify                                     │
│   → orioledb_page_wal_leaf_* (已有，block-keyed FPI)       │──────┐
│   → add_modify_wal_record (已有，CONTAINER 给 R14)          │      │
│                                                           │      │
│ root_split / tree_init / extent_ensure(on_root):          │      │
│   → XLogInsert TREE_MANIFEST  ← B.5 新增，仅 root 变化点     │──┐   │
└───────────────────────────────────────────────────────────┘  │   │
                                                               ▼   ▼
                                                          ┌─ safekeeper ─┐
                                                          └───────┬───────┘
                                                                  ▼
┌─ walingest ────────────────────────────────────────────────────┐
│ on PAGE_IMAGE rmid=129 → Value::Image 存 (rel, blkno) key      │  (已有)
│ on TREE_MANIFEST rmid=129 → OrioleDBColdStartSummary 更新       │  ← B.5 新增
│    tree_map[(datoid,relnode)] = {chkpNum, rootDownlink, ...}   │
└────────────────────────────────────────────────────────────────┘
                                                                  ▼
┌─ basebackup ───────────────────────────────────────────────────┐
│ global/orioledb.state 里包 summary v3（B.3 管道已在，schema 扩）  │
└────────────────────────────────────────────────────────────────┘
                                                                  ▼
┌─ compute cold-start ───────────────────────────────────────────┐
│ apply_orioledb_cold_start_summary（checkpoint.c:388，已在）      │
│   + 读 tree_map 字段 → 绕开 sys_trees_load_control_if_deferred  │
│   + 把 tree 的 {chkpNum, rootDownlink} 灌进 shmem               │
│   + evictable_tree_init_meta 后续用这份 shmem 读 root FPI       │
└────────────────────────────────────────────────────────────────┘
```

## 新 WAL 记录：`ORIOLEDB_XLOG_TREE_MANIFEST`

info byte 建议 `0xA0`（当前未用；见 `pgxn/orioledb/include/btree/page_walrecord.h`）。

**payload 布局**（固定 40 字节，单树一条）：

```c
typedef struct
{
    Oid     datoid;           /*  4 */
    Oid     relnode;          /*  4 */
    uint32  chkpNum;          /*  4 */
    uint32  _pad;             /*  4 */
    uint64  rootDownlink;     /*  8 */
    uint64  datafileLength0;  /*  8 */  /* slot 0 */
    uint64  datafileLength1;  /*  8 */  /* slot 1 */
} OrioleDBTreeManifestRecord;             /* total 40 */
```

**发射点（仅在 root 物理位置变化时）：**

| 位置 | 触发 | 文件 |
|---|---|---|
| `o_btree_init` 创建新 tree 之后 | 初始 root 落盘位置确定 | `btree.c:53` |
| `insert.c:212` 根分裂 `init_new_btree_page(rootPageBlkno, ...)` 之后 | 根页分裂导致 root 物理搬迁 | `btree/insert.c` |
| `checkpoint_map_write_header` 内（保留）| 兼容性冗余，checkpoint 时也发一次 | `checkpoint.c:2991` |

每条 record 针对**一棵树**。并发事务各发各的，walingest 串行消化。

### 为什么不是 "every extent ensure"

`orioledb_page_ensure_extent` 在每次 page-level FPI emit 时可能调到（page_wal.c:249,254,304 等），per-row 级别。在这里发 manifest 会导致每次 INSERT 都多一条 40 字节 WAL → **不可接受**。

mutual insight：只有 **root 的** fileExtent / rootDownlink 变化对 cold-start 重要。其他 page 的 extent 变化已经由 page-level LEAF_* FPI 在 PageServer 侧被记录（walingest 存 `(rel, blkno)` 即可），不需要 manifest 记录。

## Summary v3 wire format

基于 v2（48 字节，`libs/wal_decoder/src/orioledb_state.rs`）向后兼容扩展。

```
+--------+--------+--------+--------+
| magic  | 'O' 'R' 'O' 'S'          |   4 字节
+--------+--------+--------+--------+
| ver=3  |  flags |  _pad (2 bytes) |   4 字节
+--------+--------+--------+--------+
| next_oxid                         |   8  (v2 已有)
+-----------------------------------+
| last_pg_xid_seen | _pad (4)       |   8  (v2 已有)
+-----------------------------------+
| next_csn                          |   8  (v2 已有)
+-----------------------------------+
| last_ingested_lsn_raw             |   8  (v2 已有)
+-----------------------------------+
| ingested_count                    |   8  (v2 已有)
+-----------------------------------+
| reserved_v2 (8 bytes align)       |   8  (v2 已有的 reserved)
+===================================+   — v2 头部 56 字节 end —
| CTRL: last_checkpoint_number      |   4
| CTRL: _pad (4)                    |   4
| CTRL: control_replay_start_ptr    |   8
| CTRL: control_sys_trees_start_ptr |   8
| CTRL: control_toast_consistent_ptr|   8
| CTRL: checkpoint_retain_xmin      |   8
| CTRL: checkpoint_retain_xmax      |   8
+===================================+   — CTRL 48 字节 end —
| UNDO[0]: last_undo_location       |   8
| UNDO[0]: retain_start_location    |   8
| UNDO[0]: retain_end_location      |   8
| UNDO[1]: ... (3 × 24 = 72 bytes)  |
| UNDO[2]: ... (24 bytes)           |
+===================================+   — UNDO 72 字节 end —
| TREE_COUNT (u32) | _pad (u32)     |   8
+-----------------------------------+
| TreeEntry[0] (40 bytes)           |
| TreeEntry[1] ...                  |
| ...                               |
+===================================+   — 变长 end —
```

**固定头部总长**：56（v2）+ 48（CTRL）+ 72（UNDO）+ 8（TREE_COUNT） = **184 字节**

**变长尾部**：`40 × TREE_COUNT`

**典型大小估算**：
- 只有 sys-tree（~20 persistence tree）：184 + 800 = 984 字节
- 加 100 用户表（每表 2 tree=ctid+toast）= ~220 棵树：184 + 8800 ≈ 9KB
- `ORIOLEDB_STATE_KEY` 目前在 pageserver_api 是单 Key，单 value 大小受 PageServer 限制（应 < 16 MB，充裕）

### 兼容性

- v2 reader 见到 `version=3` 应**拒绝加载**（让 compute 回退到 Plan E 镜像读控制文件），不走 partial parse。
- 磁盘格式 bump 走标准 v 号阶梯：`OrioleDBColdStartSummary::VERSION = 3`。
- 每次 struct 变化走 `StaticAssertDecl` + panic on mismatch。

## 发射端改动清单

| 文件 | 改动 |
|---|---|
| `pgxn/orioledb/include/btree/page_walrecord.h` | 加 `ORIOLEDB_XLOG_TREE_MANIFEST (0xA0)` info byte 常量 + `OrioleDBTreeManifestRecord` struct |
| `pgxn/orioledb/src/btree/page_wal.c` | 新 `orioledb_page_wal_emit_tree_manifest(BTreeDescr *, uint32 chkpNum)` 函数 |
| `pgxn/orioledb/src/btree/btree.c:92` | `o_btree_init` 末尾新 tree FPI 后紧跟 manifest 发射 |
| `pgxn/orioledb/src/btree/insert.c:212` 附近 | root split 完成、`fileExtent` 归位后发 manifest |
| `pgxn/orioledb/src/checkpoint/checkpoint.c:2991` | `checkpoint_map_write_header` 内部除写 INIT fork FPI 外，也发一条 manifest（冗余但保证 checkpoint 时一致性）|
| `pgxn/orioledb/src/btree/page_redo.c` | `orioledb_page_redo` 新增 `TREE_MANIFEST` case：对 walredo light-mode 里的 replay 是 no-op（这条记录不改 page，只更 summary）|

## Walingest 改动清单

| 文件 | 改动 |
|---|---|
| `libs/wal_decoder/src/orioledb_state.rs` | 升 wire format 到 v3；`OrioleDbRecordDelta` 增 `tree_manifest: Option<TreeManifestDelta>`；`OrioleDBColdStartSummary` 增 `ctrl: ControlSummary`、`undo: [UndoInfo; 3]`、`tree_map: BTreeMap<(u32,u32), TreeEntry>` |
| `libs/wal_decoder/src/decoder.rs` | rmid=129 分支新 match TREE_MANIFEST info byte → 返回 `OrioleDbRecordDelta` 带 manifest 字段 |
| `pageserver/src/walingest.rs` | on `MetadataRecord::OrioleDb(delta)`: 如 `delta.tree_manifest` present, 合并进 `oriole_summary.tree_map`; 类似现 csn/oxid 单调 bump |

## Compute 端改动清单

| 文件 | 改动 |
|---|---|
| `pgxn/orioledb/src/checkpoint/control.c` | `OrioleDBStatePacked` struct 扩到 v3 layout；`apply_orioledb_cold_start_summary` 读 ctrl + undo + tree_map 并直灌进 `checkpoint_state` / `undo_meta` / 合成 tree manifest cache |
| `pgxn/orioledb/src/checkpoint/checkpoint.c:334` | `sys_trees_load_control_if_deferred` 的 fallback：如果 `get_checkpoint_control_data` 失败但 summary v3 有 ctrl 字段，使用 summary 的值 |
| `pgxn/orioledb/src/catalog/sys_trees.c:748` | 同上路径 |

## I1-I5 对齐

| Invariant | v3 设计如何满足 |
|---|---|
| I1 persistence | summary 经 ORIOLEDB_STATE_KEY 持久化；basebackup 投递（已有 B.3 管道）|
| I2 per-record pure redo | manifest 记录是 pure function（不 touch 任何 page；只更 shmem 元数据）。walredo light-mode 对它是 no-op |
| I3 materializable | tree root 的实际 page 内容仍然靠 block-keyed page-level FPI 物化；manifest 只负责告诉 compute "root 在哪个 block" |
| I4 zero replay on compute | compute 不再依赖 CONTAINER replay 或 checkpoint-emitted FPI 来找 root |
| I5 transaction atomicity | manifest 在 root-变化时发射，root 变化本身就是受 WAL ordering 保护的事件；commit barrier（A.6）保证 XLogFlush 到 safekeeper |

## 实施分解

| 步骤 | 范围 | DoD |
|---|---|---|
| B.5.1 | 新 WAL 记录类型 + struct（orioledb.h / page_walrecord.h） | build 通，rmid=129 info byte 不冲突 |
| B.5.2 | 发射端：`o_btree_init` + root split + `checkpoint_map_write_header` | 手工跑 CRUD 观察 WAL 流里出现 manifest 记录 |
| B.5.3 | wal_decoder v3 struct + TREE_MANIFEST decode | 单测：构造一条 manifest record，decode 得到正确 `TreeManifestDelta` |
| B.5.4 | pageserver walingest 合并 tree_map | 单测：多条 manifest 按 LSN 顺序合并，最后状态正确 |
| B.5.5 | basebackup 投递 summary v3 | end-to-end：`global/orioledb.state` 文件里能解析出 tree_map 非空 |
| B.5.6 | compute apply: 读 summary v3 tree_map → 填 shmem | end-to-end：`test_e2e_crash_concurrent.sh` [5/10] 段不再撞 `Assert("o_table")` |
| B.5.7 | (可选) 清理 `sys_trees_load_control_if_deferred` 回退路径 | 确认 summary v3 走通后再删 |

## 不做的事（避免膨胀）

- **不**在 summary 里放全部 sys-tree 页内容——只放 manifest。页内容仍走 block-keyed FPI 从 PageServer 拉。
- **不**取消 `checkpoint_map_write_header` / `write_checkpoint_control` ——它们作为冗余 + 压缩 map file 的机制保留，不在 Phase 3 scope。
- **不**做"每条 mutation 发 manifest"——per-row cost 不可接受，只在 root-变化时发。

## 开放问题（实施前需决定）

1. **manifest record 是否需要 commit-barrier 保护**？A.6 的 XLogFlush on commit 已经保证 commit 涉及的所有 rmid=129 记录到 safekeeper。manifest 只在 root-split 时发，root-split 发生在持锁的 insert 路径里，理论上跟随下一次 commit 的 flush。需实验验证。
2. **root 位置在 compute 运行期会不会"回退"**？checkpoint 的 COW 会让 root 换位，但永远单调前进（chkpNum 单调）；manifest 的 `chkpNum` 字段作为 LSN-等效用于合并冲突。
3. **worker-pool 路径的 manifest**：`recovery/worker.c` 在 signal-path retire 后会死，但过渡期发射不能少。加一个 `!is_recovery_process()` guard。

---

**下一步**：这份 schema 你审一眼确认方向，然后开始 B.5.1（WAL record 类型）。B.5.1 是小工作量（<50 行 C），跑通就有正向反馈。
