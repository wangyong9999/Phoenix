# OrioleDB-on-Neon — Execution Plan

> **Status:** v1.1 — 2026-04-21.
>
> **Purpose:** 把 "设计问题 Q1–Q5" 与 "实施接口 E1–E4" 之间的协同关系、
> 门禁、风险点写清楚，作为后续每个 PR 的上位 checklist。
>
> **Authority:** 本文件是项目级推进图。具体实现细节对
> `ENTERPRISE_HARDENING_PLAN.md`；架构契约对 `INVARIANTS.md` v1.0；
> 设计问题对 `MVP_FIRST_PRINCIPLES.md`。
>
> **Binding rule:** 每个 PR 的 commit message 必须陈述对 I1–I5 的影响，
> 以及所属 Phase/Track。违反者不合并。
>
> **v1.1 relative to v1.0:**
> - Phase 1 分成 1a/1b/1c，只有 1a 是 Phase 2.1 硬前置；1b 只对 Phase 2.3。
> - Phase 2 分成 2.1（I4 关键路径，串行）/ 2.2（并行硬化）/ 2.3（语义粒度）。
> - A.6 从"条件性"升为硬前置（P1.6 审出违反，commit 1434272 已落）。
> - R4 风险关闭。
> - 把原 Phase 2.5 Track C 并入 Phase 2.1。

---

## 0 — 两个正交维度

项目推进沿两个维度同时进行：

- **Q1–Q5 设计问题**：输出是对齐 I1–I5 的决策文档，不含代码。Q 之间有依赖关系。
- **E1–E4 实施接口**：输出是落在对应代码路径上的加法式改动。每个 E 内部可独立开发，但 DoD 依赖一个或多个 Q 的决策。

Q 先于 E。Q 不闭合前启动 E，就是历次翻车的根因。但 **不是所有 Q 都在关键路径上**——详见 §2/§3 的分层门禁。

### 0.1 Q1–Q5 索引

| Q | 主题 | 输出位置 | 状态 |
|---|---|---|---|
| Q1 | 事件 schema closure | `Q1_EVENT_CLOSURE_AUDIT.md` v1.0 | ✅ commit 8922415 |
| Q2 | per-event redo 纯函数契约 | `Q2_REDO_CONTRACT.md`（待建） | ⏳ |
| Q3 | base image 生命线 | `Q3_BASE_LIFELINE.md`（待建） | ⏳ |
| Q4 | emit-time IMAGE/DELTA 信号 | `Q4_EMIT_DECISION.md`（待建） | ⏳ |
| Q5 | 冷启动 state sources | `Q5_COLDSTART_SOURCES.md` v0.1 | ✅ commit a530934 |
| I5-write 审 | commit path barrier | `P1_6_I5_WRITE_AUDIT.md` v1.0 | ✅ commit abb7e2e（**违反确认**） |

### 0.2 E1–E4 索引

| E | 方向 | 稳态/恢复 | 主要代码路径 |
|---|---|---|---|
| E1 | 生成（compute → WAL） | 稳态 | `pgxn/orioledb/src/btree/page_wal.c`、`recovery/wal.c`、`transam/undo.c` commit path |
| E2 | 物化（WAL → PageServer state） | 稳态 | `libs/wal_decoder`、`pageserver/src/walingest.rs`、`pageserver/src/walredo/*`、`tenant/storage_layer/*` |
| E3 | 冷启动（basebackup Path A） | 恢复 | `libs/postgres_ffi/src/xlog_utils.rs`、`pageserver/src/basebackup*`、`compute_tools/src/compute.rs` |
| E4 | 运行读（GetPage Path B） | 稳态 | `pageserver/src/page_service.rs`、compute smgr / neon page-fetch 客户端 |

"稳态" = 正常运行持续触发；"恢复" = 只在 cold-start / restart / branch 时触发。

---

## 1 — Q × E 协同矩阵

`◉` = 直接耦合；`◯` = 间接耦合；`–` = 无关。

| | E1 生成 | E2 物化 | E3 冷启动 | E4 运行读 |
|---|---|---|---|---|
| **Q1** 事件集合 | ◉ emit 点定义 | ◉ dispatch + redo 选择 | ◯ 经 E2 | ◯ 经 E2 |
| **Q2** redo 契约 | ◉ emit-side payload 结构 | ◉ redo-side 纯函数 | – | ◯ 经 E2 walredo |
| **Q3** base 生命线 | ◉ base 发射（PAGE_IMAGE 时机） | ◉ compaction 合并 | ◯ 经 E2 | ◉ 链长消费者 |
| **Q4** emit 决策 | ◉ 唯一责任 | – | – | – |
| **Q5** 冷启动源 | ◯ commit 路径产 summary 源头 | ◉ walingest summary 维护 | ◉ 投递 + 消费 | – |
| **I1** log 持久 | ◉ | ◉ | – | – |
| **I2** per-record | ◉ | ◉ | – | ◯ |
| **I3** materializable | ◯ base 供应 | ◉ 实现 | ◯ 依赖 E2 输出 | ◉ 消费 |
| **I4** 零 replay | – | – | ◉ Path A | ◉ Path B |
| **I5-write** | ◉ | – | – | – |
| **I5-read** | – | ◯ xidmap 物化 | – | ◉ |

关键观察（v1.0 不变）：
- Q1 是全局根。
- Q2 是 E1↔E2 契约绑定。
- Q3 双方产出、E4 消费。
- Q4 内聚 E1；Q5 跨 E2–E3。

---

## 2 — Phase 1：设计闭包（文档，不动代码）

**分层门禁（v1.1 新增）：**

- **Phase 1a** 是 Phase 2.1 I4 关键路径的硬前置，必须先闭合。
- **Phase 1b** 只是 Phase 2.3 语义粒度的前置，可以与 Phase 2.1 **并行**推进，不阻塞 Phase 2.1 开工。
- **Phase 1c** 是文档一致性收尾，任何时候做都行。

### 2.1 Phase 1a — Phase 2.1 硬前置

| # | 任务 | 状态 | Commit |
|---|---|---|---|
| P1.1 | Q1 → v1.0（N1–N5 结果整合） | ✅ | 8922415 |
| P1.5 | `Q5_COLDSTART_SOURCES.md` v0.1（shmem 标量 + per-tree counters 归档） | ✅ | a530934 |
| P1.6 | I5-write 审计 | ✅ | abb7e2e — 违反确认 |

**Phase 1a 已全部闭合。Phase 2.1 不再阻塞于设计问题。**

### 2.2 Phase 1b — Phase 2.3 前置（可与 Phase 2.1 并行）

| # | 任务 | 前置 | DoD |
|---|---|---|---|
| P1.2 | Q2 草案 | P1.1 | 每个事件类型有 payload schema + redo 函数签名（含 LEAF_\*/SPLIT/MERGE/COMPACT/UNDO_APPLY/PAGE_INIT/PAGE_IMAGE/CONTAINER） |
| P1.3 | Q3 策略选型 | P1.2 | 策略 A/B/C/D 中选定；链长上界写清 |
| P1.4 | Q4 信号选型 | P1.3 | S1/S4 选定；in-memory checkpointNum 更新方案（如选 S1） |

**关键调整（v1.1）**：P1.2–P1.4 **不再阻塞** Phase 2.1 启动。Phase 2.1 的工作（B.3/B.4/C.x）只依赖 Q1 + Q5；Q2/Q3/Q4 是 Phase 2.3 的输入。

### 2.3 Phase 1c — 一致性收尾

| # | 任务 | DoD |
|---|---|---|
| P1.7 | `MVP_FIRST_PRINCIPLES.md §1` 从 I1–I4 同步到 I1–I5 | 文档对齐 |

小工作，任何时候。

### 2.4 Phase 1 输出物（v1.1 状态）

| 文件 | 状态 |
|---|---|
| `Q1_EVENT_CLOSURE_AUDIT.md` v1.0 | ✅ |
| `Q5_COLDSTART_SOURCES.md` v0.1 | ✅ |
| `P1_6_I5_WRITE_AUDIT.md` v1.0 | ✅ |
| `INVARIANTS.md §8` Audit #1 关闭 | ✅ |
| `Q2_REDO_CONTRACT.md` | ⏳ |
| `Q3_BASE_LIFELINE.md` | ⏳ |
| `Q4_EMIT_DECISION.md` | ⏳ |
| `MVP_FIRST_PRINCIPLES.md §1` I5 同步 | ⏳ |

---

## 3 — Phase 2：三段式执行

**v1.1 关键重构：** 之前的 "Track A/B/F 对等并行" 不反映实际关键路径。正确的顺序是：
1. **Phase 2.1 I4 关键路径** —— 从 walingest summary 到 compute 新 codepath 到退 signal，串行推进，最快路径关掉 I4 违规。
2. **Phase 2.2 并行硬化** —— 跟 2.1 独立，只读/只测/纯 PageServer 内部，发现问题反哺 2.1 决策。
3. **Phase 2.3 语义粒度** —— Phase 2.1 稳定后才启动，改 emit shape 会同时牵动 walingest 和 walredo，不在关键路径上。

### 3.1 Phase 2.1 — I4 关键路径（串行推进）

**目标**：让 compute 冷启动完全不经 `orioledb_recovery.signal`，退掉 signal + selective replay 的 I4 违规。

**硬前置**：
- Phase 1a 全部闭合（已满足）。
- **A.6 XLogFlush barrier 必须在 Phase 2.1 启动前落地**（commit 1434272 已完成）。

**串行顺序**：

```
A.6 (✅ commit 1434272)
  ↓
B.3  walingest OrioleDB-state summary 结构 + 基础字段（按 Q5 v0.1）
  ↓
B.4  summary 扩展 Q5 全量字段（Categories A + B）
  ↓
C.1  basebackup summary 载体定型（独立 blob，不改 pg_control）
  ↓
C.2  basebackup 端生成流
  ↓
C.3  compute 新 codepath 读 summary 初始化 shmem
  ↓
C.4  feature flag（新路径 opt-in）
  ↓
[观察期 ≥ 2 周，empirical 验证 cold-start 一致性]
  ↓
flip default → new path
  ↓
P3.1  删 compute 侧 signal + pg_wal copy
P3.2  降级 apply_btree_modify_record（保留 logical decoding 消费）
P3.3  删 orioledb_recovery.signal 生成端
```

**Phase 2.1 DoD**：
- [ ] walingest summary 可独立重建 = 运行态 shmem 状态（差分测试）
- [ ] basebackup 投递 summary 机制 E2E 可用
- [ ] compute 新 codepath 经 feature flag 启用后能启动并服务查询
- [ ] 冷启动矩阵通过（多 checkpoint 周期后）
- [ ] signal/selective-replay 路径删除后，crash-restart 测试全绿
- [ ] CONTAINER emit 保留（R14 logical decoding）

### 3.2 Phase 2.2 — 并行硬化（与 2.1 独立）

**目标**：验证稳态读取路径在 OrioleDB workload 下的真实行为。均为**只读/只测/PageServer 内部**，不改 emit shape，可跟 2.1 完全并行。

| # | 子任务 | 属 E | Q 依据 |
|---|---|---|---|
| F.1 | OrioleDB 事件的 delta chain GetPage 压测 | E4 | Q3, I3 |
| F.2 | sys-tree 首访 `wait_lsn` 60s ceiling 实测 | E4 | I4 Path B |
| F.3 | Branch/PITR 到 mid-txn LSN 的 I5-read 测试 | E4 | I5-read |
| F.4 | GetPage 超时边界降级行为审视 | E4 | Q3 |
| B.1 | walredo per-event handler 骨架（对 FPI 事件 trivial identity） | E2 | Q1 |
| B.2 | walredo C 扩展加载验证（`libs/orioledb_walredo`） | E2 | Q2 |
| B.5 | Layer compaction 在 OrioleDB keyspace 端到端验证 | E2 | Q3, I3 |

**Phase 2.2 DoD**：
- [ ] OrioleDB workload 下 GetPage 压测通过 + empirical 链长有界
- [ ] sys-tree 首访延迟 < 60s 在目标 workload
- [ ] I5-read branch mid-txn 快照全可见 xor 全不可见
- [ ] Layer compaction 在 OrioleDB keyspace 下不丢不坏

### 3.3 Phase 2.3 — 语义粒度（Phase 2.1 稳定后）

**目标**：把 E1 从"所有事件都 FPI"改进到"DELTA/IMAGE 混合编码按 Q4 决策"，为 Git-for-Data 的语义粒度铺路。

**门禁**：Phase 2.1 flip default + ≥ 2 周稳态 + Phase 1b 闭合（Q2/Q3/Q4 决策出具）。

| # | 子任务 | 属 E | Q 依据 |
|---|---|---|---|
| A.1 | SPLIT/MERGE/COMPACT 的 DELTA 编码实现 | E1 | Q2 |
| A.2 | UNDO_APPLY 的纯函数 payload 设计 | E1 | Q2 |
| A.3 | PAGE_INIT 激活或明确保留（G4 收尾） | E1 | Q1 G4, Q2 |
| A.4 | Q4 S1 emit 决策信号实现 | E1 | Q4 |
| A.5 | In-memory `checkpointNum` 更新 gap 修复（S1 必备） | E1 | Q4 |
| B.1' | walredo per-event handler 从 identity 升级为 DELTA 纯函数 | E2 | Q2 |

**Phase 2.3 DoD**：
- [ ] 每个 emit 点标注对应 Q2 payload schema 版本
- [ ] Q1 事件集合全覆盖（含保留槽激活 or 明确保留）
- [ ] Q4 emit 决策 live；in-memory checkpointNum gap 修复
- [ ] Git-for-Data 语义切换测试：按单事件 replay 正确

---

## 4 — Phase 3：长尾清理 + 测试

Phase 3 的主体 retirement（P3.1–P3.3）已归并进 Phase 2.1 末尾。本节专治长尾。

**扩展测试**：
- N8 crash 矩阵加 branch/PITR mid-commit 场景
- 热页面 workload 下 I3 chain length empirical 上界
- sys-tree 首访延迟的长尾分布

**可选清理**：
- ROOT_SPLIT / PAGE_INIT / LEAF_LOCK 预留 enum 保留或正式移除（Phase 2.3 A.3 决策）
- M1.2/M1.3 in-code 注释里对 "XACT record's XLogFlush" 的错误表述（随 A.6 落盘应该同步修正，可另起一个小 commit）

---

## 5 — 纪律点

1. **每个 PR 的 commit message 必须陈述对 I1–I5 的影响**（"no invariant impact" 也行）。CLAUDE.md 硬性要求。
2. **Phase 2.1 门禁**：Phase 1a 闭合 + A.6 落地。**不要求** Phase 1b 闭合。
3. **Phase 2.3 门禁**：Phase 1b 闭合 + Phase 2.1 稳定 ≥ 2 周。
4. **Phase 2 期间不得改 CONTAINER 编码**。R14 logical decoding 的 source of truth。
5. **Q-answer 的修改必须回头审 E1–E4 每条 track 对应子项是否仍对齐**。不允许静默修订；走版本升位 + change log。
6. **Phase 2.1 的 signal retirement 前置清单**：A.6 ✅ + B.3/B.4 + C.1–C.3 + flip 默认 + 观察期 ≥ 2 周。缺一项不得动刀 P3.x。
7. **每条 track 的增量都要是 feature-flag 能关掉的形态**；Phase 3 才是拆老路径。

---

## 6 — 风险登记

| # | 风险 | 触发点 | 状态 | Fallback |
|---|---|---|---|---|
| R1 | Q2 审出某事件 redo **不可纯化** | Phase 1b P1.2 | 未触发 | 若 SPLIT/MERGE 不可纯：退化为 PAGE_IMAGE；若 UNDO_APPLY 不可纯：继续用 FPI；若 CONTAINER 不可纯：Phase 2.1 末只降级不删 |
| R2 | Q3 实测链长 **unbounded** | Phase 2.2 B.5 | 未触发 | 强制 Strategy B（first-write-after-checkpoint 自动 FPI），放弃纯 C（layer compaction）路径 |
| R3 | Layer compaction 在 OrioleDB keyspace **不工作** | Phase 2.2 B.5 | 未触发 | 放弃 C，采 Strategy B；若 B 实现复杂，MVP 暂用 Strategy A + 缩短 checkpoint 周期 |
| ~~R4~~ | ~~I5-write 审出 barrier 顺序违反~~ | Phase 1a P1.6 | **Closed 2026-04-21** | 违反确认（P1_6_I5_WRITE_AUDIT.md），A.6 已落 commit 1434272，Phase 2.1 启动不再受阻 |
| R5 | wait_lsn 60s ceiling **不够 sys-tree 首访** | Phase 2.2 F.2 | 未触发 | 调 `wait_lsn_timeout`；或在 E3 冷启动阶段预加载 sys-tree meta 页 |
| R6 | Q5 发现某 shmem 标量**既非 sys-tree 可得也非 summary 可重建** | Phase 1a P1.5 | **Closed 2026-04-21** | Q5 v0.1 Categories A+B 全部 summary-可重建；R.1–R.4 residuals 不在关键路径 |
| R7 | Phase 2.1 signal retirement 后 logical decoding 回归 | P3.2 | 未触发 | P3.2 降级不删：`apply_btree_modify_record` 仍编译进 binary，但新 codepath 不调用；R14 独立 consumer 保留 |
| **R8** (new) | A.6 XLogFlush 未充分验证（E2E 环境不完整） | A.6 本次 commit | Open | Phase 2.1 启动前在完整环境（含 storage controller）回归跑 `test_e2e_crash_concurrent.sh` 等 crash 矩阵，empirical 确认 |

---

## 7 — 当前推进态（快照，v1.1 截面）

```
Phase 1a ✅ (P1.1 / P1.5 / P1.6 全闭合)
Phase 1b ⏳ (Q2/Q3/Q4 pending，但不阻塞)
Phase 1c ⏳ (P1.7，小工作)

Phase 2.1 ⏸ (A.6 ✅ commit 1434272，B.3 尚未启动)
Phase 2.2 ⏸ (可随 Phase 2.1 并行启动)
Phase 2.3 ⏸ (等 Phase 2.1 稳定)

Phase 3 归并 Phase 2.1 末段
```

**下一步候选**：B.3 walingest OrioleDB-state summary 结构设计与落地。其余 Phase 2.1 任务依次推进。

---

## 8 — 变更日志

- **v1.0 (2026-04-21)** — 初稿。Q（设计问题）× E（实施接口）两维度。Phase 1–3 门禁 + DoD + 风险登记。
- **v1.1 (2026-04-21)** — Phase 1 拆成 1a/1b/1c，只要 1a 是 Phase 2.1 硬前置。Phase 2 重构为 2.1（I4 关键路径串行）/ 2.2（并行硬化）/ 2.3（语义粒度）。原 Phase 2.5 Track C 并入 Phase 2.1。A.6 从条件性升为硬前置并标记 ✅ commit 1434272。R4 + R6 关闭，新增 R8（A.6 empirical 验证待完整环境回归）。加 §7 "当前推进态"快照。
