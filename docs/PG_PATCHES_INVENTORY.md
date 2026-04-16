# PG Patches Inventory — Neon + OrioleDB 融合

## 1. Neon 对 PG 的改造（241 个文件）

### 核心改造（影响架构的）

| 改造领域 | 关键文件 | 改动内容 | 目的 |
|---------|---------|---------|------|
| **smgr 可插拔化** | `smgr.h`, `smgr.c` | 新增 `f_smgr` 回调结构体 + `smgr_hook` | 让存储 I/O 可被扩展替换为 PageServer |
| **LSN 追踪** | `xlog.h`, `xlog.c` | 新增 6 个 `set_lwlsn_*` hooks | 追踪每个页面最后写入的 LSN |
| **LWLock 扩展** | `lwlocklist.h` | 新增 `LastWrittenLsnLock` | LSN 缓存并发控制 |
| **中断回调** | `miscadmin.h`, `postgres.c` | 新增 `ProcessInterruptsCallback` | 扩展可插入中断处理逻辑 |
| **Prefetch 基础设施** | `instrument.h`, `execnodes.h`, `nbtree.h` | `BufferUsage.prefetch`, `BTScanOpaqueData` 新字段 | B-tree 和 Bitmap 扫描预取优化 |
| **数据库大小 hook** | `smgr.h` | `dbsize_hook_type` | 允许扩展自定义数据库大小计算 |
| **SLRU 远程读** | `slru.c`, `smgr.h` | `smgr_read_slru_segment()` | CLOG/MultiXact 可通过 PageServer 读取 |
| **全局标志** | `globals.c`, `miscadmin.h` | `neon_use_communicator_worker` | 控制 Neon 通信器工作模式 |

### 性能优化改造

| 文件 | 内容 |
|------|------|
| `nbtsearch.c` (255 行) | B-tree 叶子页面预取 |
| `nodeBitmapHeapscan.c` (283 行) | Bitmap Heap Scan 预取优化 |
| `vacuumlazy.c` (204 行) | VACUUM 性能优化 |
| `xlogreader.c` (190 行) | WAL 读取优化 |
| `xlogrecovery.c` (278 行) | WAL recovery 优化 |

### 安全修复（PG 17.8 backport）

pg_dump 注入修复、pgcrypto 溢出修复、认证改进等 — 这些跟架构无关，是上游安全 patch。

---

## 2. OrioleDB 对 PG 的改造（208 个文件）

### 核心改造（影响架构的）

| 改造领域 | 关键文件 | 改动内容 | 目的 |
|---------|---------|---------|------|
| **CSN 数据类型** | `c.h`, `transam.h` | 新增 `CommitSeqNo` typedef (uint64) | 基于 CSN 的 MVCC |
| **Snapshot 扩展** | `snapshot.h`, `snapmgr.c` | `SnapshotData` 加 `CSNSnapshotData` 字段 + hooks | 支持 CSN 快照和 hooks |
| **TableAM 增强** | `tableam.h`, `tableam.c` | 30+ 个回调扩展（tupleid→Datum, lock 增强等） | 支持非 heap 存储引擎 |
| **Index AM 增强** | `amapi.h`, `genam.c` | `amreuse`, `amupdate`, `amdelete`, `aminsertextended` | 索引支持 update/delete/reuse |
| **Custom TOAST** | `toast_internals.c`, `trigger.h` | TOAST 抽象化, `tcs_private` 字段 | 存储引擎自定义 TOAST |
| **Checkpoint hook** | `xlog.h`, `xlog.c` | `CheckPoint_hook_type` | 扩展可在 checkpoint 时执行逻辑 |
| **Recovery hooks** | `xlog.h`, `xlogrecovery.c` | `RedoShutdownHook`, 恢复相关 hooks | 扩展可参与 recovery 流程 |
| **Error cleanup hook** | `elog.h`, `elog.c` | 错误清理回调 | 扩展可在错误时清理资源 |
| **Cache invalidation** | `inval.c`, `catcache.c` | `SearchCatCacheInternal_hook` 等 | 扩展可自定义目录缓存 |
| **Row ID 抽象** | `execnodes.h`, `nodeModifyTable.c` | `tupleid` 从 `ItemPointer` 改为 `Datum` | 支持非 CTID 的行标识 |
| **Collation hook** | `pg_locale.h`, `pg_locale.c` | `pg_newlocale_from_collation_hook` | 扩展可自定义 collation |
| **Custom BGWORKER** | 多个文件 | `BGWORKER_CLASS_SYSTEM` | 系统级后台 worker |
| **Rewind 扩展** | `pg_rewind.c` | `--extension-rewind` 选项 | 存储引擎自定义 rewind 逻辑 |
| **Assert hooks** | `relcache.h`, `bufmgr.h` | `AssertCouldGetRelation`, `AssertBufferLocksPermitCatalogRead` | 存储引擎自定义 assert |

### Executor 层改造（最大的改动区域）

| 文件 | 行数 | 内容 |
|------|------|------|
| `nodeModifyTable.c` | ~900 行 | tuple locking API 全面重构（Datum tupleid） |
| `trigger.c` | ~300 行 | 适配新的 TOAST 和 tuple 锁定 |
| `execReplication.c` | ~200 行 | 逻辑复制适配 |
| `execMain.c` | ~150 行 | 执行器适配 |

---

## 3. 融合处理分类

### 3.1 无冲突直接复用（148 个文件）

Neon 改了但 OrioleDB 没改，或 OrioleDB 改了但 Neon 没改 — 直接使用对应版本。

### 3.2 自动三方合并成功（53 个文件）

两边都改了但改动区域不重叠 — `diff3` 自动合并。

### 3.3 需手动处理（5 个文件）

| 文件 | 冲突原因 | 处理方式 |
|------|---------|---------|
| `xlog.c` | Neon 加 lwlsn hooks + OrioleDB 加 checkpoint hooks，位置相邻 | 保留两边（追加） |
| `nodeModifyTable.c` | OrioleDB 大量重构 + Neon 安全修复 | 用 OrioleDB 版本（改动更大更核心） |
| `catcache.c` | Neon 无改动 + OrioleDB 加 hook | 保留 OrioleDB hook |
| `inval.c` | 类似 catcache.c | 保留 OrioleDB hook |
| `pgbench.c` | OrioleDB 加 custom AM 支持 + Neon 小改 | 用 OrioleDB 版本 |

### 3.4 额外适配（手动补丁）

| 项目 | 内容 |
|------|------|
| `database_size_hook_type` 别名 | Neon 叫 `dbsize_hook_type`，OrioleDB 叫 `database_size_hook_type`，加了 typedef 别名 |

---

## 4. 后续 PG 社区演进的跟随能力

### 4.1 Neon 的改造 — 跟随性好

| 特征 | 评估 |
|------|------|
| 改造方式 | 主要通过 **hook** 机制，不改 PG 内部逻辑 |
| 侵入性 | **低** — smgr_hook 是 PG 标准扩展点 |
| 上游合入前景 | smgr 可插拔化已在 PG 社区讨论中 |
| PG 大版本升级 | 主要工作是 rebase hook 定义，通常 1-2 天 |
| 风险 | **低** — PG 不太会删除已有的 hook 基础设施 |

### 4.2 OrioleDB 的改造 — 跟随性中等

| 特征 | 评估 |
|------|------|
| 改造方式 | 大量新增 **hook** + 修改 **核心数据结构**（TableAM, IndexAM, Snapshot） |
| 侵入性 | **中高** — TableAM 回调签名变更、tupleid 类型变更影响面广 |
| 上游合入前景 | OrioleDB 团队正在推动 PG 上游合入，但进展缓慢 |
| PG 大版本升级 | 需要 2-4 周 rebase，主要是 executor 层 API 变化 |
| 风险 | **中** — PG 18 如果重构 executor 或 TableAM，patches 需要大改 |

### 4.3 融合版的跟随策略

```
PG 18 发布时的升级路径:

1. OrioleDB 团队发布 patches18_x (基于 PG 18)
   → 他们负责 rebase 自己的 patches
   
2. Neon 团队发布 REL_18_STABLE_neon (基于 PG 18)
   → 他们负责 rebase 自己的 patches

3. 我们重复三方合并流程:
   → Neon PG 18 + OrioleDB patches18_x
   → 已有的合并脚本可以复用
   → 预计 1-3 天工作量（已知 60 个交叉文件，模式固定）
```

### 4.4 最大风险点

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| PG 重构 TableAM API | 中 | 高 — OrioleDB patches 需要大改 | 跟随 OrioleDB 团队的 rebase |
| PG 重构 smgr 层 | 低 | 高 — Neon patches 需要大改 | 跟随 Neon 团队的 rebase |
| OrioleDB patches 上游合入 | 低（短期） | 正面！— 不再需要 patch | 持续关注 PG 社区进展 |
| Neon smgr hook 上游合入 | 中 | 正面！— 不再需要 patch | 持续关注 PG 社区进展 |
| 两个项目 PG 版本节奏不同步 | 中 | 中 — 需要额外 backport | 以较慢的一方为准 |

---

## 5. 文件改动量统计

```
Neon 改造:     241 个文件, ~6000+ 行改动
OrioleDB 改造: 208 个文件, ~6800 行改动
交叉文件:       60 个文件 (需要三方合并)
自动合并成功:    53 个
手动处理:        5 个 + 2 个适配补丁
```
