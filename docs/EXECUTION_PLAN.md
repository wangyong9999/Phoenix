# OrioleDB-on-Neon — Execution Plan

> **Status:** v1.0 — 初稿（2026-04-21）。
>
> **Purpose:** 把 "设计问题 Q1–Q5" 与 "实施接口 E1–E4" 之间的协同关系、门禁、风险点写清楚，作为后续每个 PR 的上位 checklist。
>
> **Authority:** 本文件是项目级的推进图。具体实现细节对 `ENTERPRISE_HARDENING_PLAN.md`；架构契约对 `INVARIANTS.md`（v1.0）；设计问题对 `MVP_FIRST_PRINCIPLES.md`。
>
> **Binding rule:** 每个 PR 的 commit message 必须陈述对 I1–I5 的影响，以及所属 Phase/Track。违反者不合并。

---

## 0 — 两个正交维度

项目推进沿两个维度同时进行：

- **Q1–Q5 设计问题**：输出是对齐 I1–I5 的决策文档，不含代码。Q 之间有依赖关系。
- **E1–E4 实施接口**：输出是落在对应代码路径上的加法式改动。每个 E 内部可独立开发，但 DoD 依赖一个或多个 Q 的决策。

Q 先于 E。Q 不闭合前启动 E，就是历次翻车的根因。

### 0.1 Q1–Q5 索引

| Q | 主题 | 输出位置 | 依赖 |
|---|---|---|---|
| Q1 | 事件 schema closure | `Q1_EVENT_CLOSURE_AUDIT.md` | – |
| Q2 | per-event redo 纯函数契约 | `Q2_REDO_CONTRACT.md`（待建） | Q1 |
| Q3 | base image 生命线 | `Q3_BASE_LIFELINE.md`（待建） | Q2 |
| Q4 | emit-time IMAGE/DELTA 信号 | `Q4_EMIT_DECISION.md`（待建） | Q3 |
| Q5 | 冷启动 state sources | `Q5_COLDSTART_SOURCES.md`（待建） | Q1（独立于 Q2/3/4） |

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

`◉` = 直接耦合（该 track 的代码直接实现 Q 的决策）；`◯` = 间接耦合（正确性经另一条 track 传递）；`–` = 无关。

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

### 1.1 关键观察

- **Q1 是全局根**。它定义 pipeline 里流动的"货物"（事件）；E1 发、E2 接、E4 间接依赖、E3 经 E2 打包。Q1 改了，E1–E4 每条 track 都要同步。
- **Q2 是 E1↔E2 的契约绑定**。emit-side 和 redo-side 必须对齐同一个 payload schema 版本。
- **Q3 是双方产出、一方消费**。E1（发射 base）+ E2（compaction）共同供给，E4 作为 GetPage 的消费者验证链长。
- **Q4 内聚于 E1**。不溢出，小步调整风险最低。
- **Q5 跨 E2–E3**。E2 产 summary，E3 投递 + 消费。

---

## 2 — Phase 1：设计闭包（文档，不动代码）

**门禁：** Phase 1 全部条目关闭前，Phase 2 任何 track 都不得启动代码修改。

Phase 1 并行图：

```
Q1 v1.0 ─┬─→ Q2 ─→ Q3 ─→ Q4      (依赖链)
         │
         ├─→ Q5                    (独立)
         │
         └─→ I5-write 审计         (独立)
              │
              └─→ MVP_FIRST_PRINCIPLES §1 同步 I1–I5
```

### 2.1 任务清单

| # | 任务 | 前置 | DoD |
|---|---|---|---|
| P1.1 | Q1 → v1.0（N1–N5 结果整合） | N1–N5 已跑完 | Q1 v1.0 发布，B33 修正，G1/G2'/G3/G5' 定性 |
| P1.2 | Q2 草案 | P1.1 | 每个事件类型有 payload schema + redo 函数签名（含 LEAF_*/SPLIT/MERGE/COMPACT/UNDO_APPLY/PAGE_INIT/PAGE_IMAGE/CONTAINER） |
| P1.3 | Q3 策略选型 | P1.2 | 策略 A/B/C/D 中选定；链长上界写清 |
| P1.4 | Q4 信号选型 | P1.3 | S1/S4 选定；in-memory checkpointNum 更新方案（如选 S1） |
| P1.5 | Q5 inventory 完工 | P1.1 | N2/N3 的 shmem 标量全部归到 summary 字段表，或解释无需（sys-tree GetPage 覆盖） |
| P1.6 | I5-write 审计 | P1.1 | 读 `current_oxid_commit` + M1.2/M1.3 + `RecordTransactionCommit` 实际顺序；结论写入 `INVARIANTS.md §8` |
| P1.7 | `MVP_FIRST_PRINCIPLES.md §1` 从 I1–I4 同步到 I1–I5 | P1.6 | 文档对齐 |

P1.2/P1.5/P1.6 可以并行启动（它们的前置都只是 P1.1）。

### 2.2 Phase 1 输出物

- `Q1_EVENT_CLOSURE_AUDIT.md` v1.0
- `Q2_REDO_CONTRACT.md`
- `Q3_BASE_LIFELINE.md`
- `Q4_EMIT_DECISION.md`
- `Q5_COLDSTART_SOURCES.md`
- `INVARIANTS.md §8` 闭合 Audit #1 (I5-write)
- `MVP_FIRST_PRINCIPLES.md §1` 同步 I5

---

## 3 — Phase 2：稳态闭环（加法式代码落地）

**门禁：** Phase 1 全部关闭；每条 track 另有自己的 Q 前置：

| Track | 接口 | 涵盖 I | 前置 Q | 最小启动条件 |
|---|---|---|---|---|
| A | E1 + I5-write | I1, I2, I5-w | Q1, Q4 | P1.1 + P1.4 + P1.6 |
| B | E2 | I2, I3 | Q1, Q2 | P1.1 + P1.2 |
| F | E4 + I5-read | I3-consume, I5-r | Q1, Q3 | P1.1 + P1.3 |

门禁意义：A/B/F 起跑时点不同，不是齐头并进。**每条 track 的每个 PR 不得引入 Q1 之外的新事件类型**。

### 3.1 Track A — E1 (生成)

**范围**：
- 为非 LEAF 事件类型实现 DELTA / IMAGE 双编码能力（按 Q2）
- 实现 Q4 选定的 emit 决策信号
- 修正 I5-write 的 commit barrier 顺序（如 P1.6 审出 gap）
- 老 CONTAINER 发射路径**不动**

**子任务**：

| # | 内容 | Q 依据 |
|---|---|---|
| A.1 | SPLIT/MERGE/COMPACT 的 DELTA 编码实现 | Q2 |
| A.2 | UNDO_APPLY 的纯函数 payload 设计 | Q2 |
| A.3 | PAGE_INIT 激活或明确保留（G4 收尾） | Q1 G4, Q2 |
| A.4 | Q4 S1（或 S4）emit 决策信号实现 | Q4 |
| A.5 | In-memory `checkpointNum` 更新 gap 修复（S1 路径必备） | Q4 |
| A.6 | I5-write barrier 顺序修正（如有 gap） | I5-write |

**DoD**：
- [ ] 每个 emit 点标注对应 Q2 payload schema 版本
- [ ] Q1 事件集合实现全覆盖（含保留槽的决策：激活 or 文档化）
- [ ] Q4 决策信号 ready，in-memory checkpointNum 更新 gap 修复
- [ ] I5-write 测试通过：mid-commit crash 场景下事务 WAL 原子到 SafeKeeper
- [ ] 老 CONTAINER 发射未动

### 3.2 Track B — E2 (物化)

**范围**：
- walredo 扩展：为每个事件类型接入 Q2 纯函数
- walingest 扩展：维护 OrioleDB-state summary
- Layer compaction 在 OrioleDB keyspace 上的端到端验证
- CONTAINER 的消费路径**保留**（logical decoding 需要），但非物化链

**子任务（三个子系统各自独立）**：

| # | 子系统 | 内容 | Q 依据 |
|---|---|---|---|
| B.1 | walredo | 为每个 Q1 事件实现对应的纯函数 handler（light mode 可运行） | Q2 |
| B.2 | walredo | 连接 `libs/orioledb_walredo`（C 扩展加载） | Q2 |
| B.3 | walingest | OrioleDB-state summary 结构定义 + 消费逻辑（从 rmid=129 各记录更新） | Q5 |
| B.4 | walingest | summary 字段覆盖 Q5 所有 shmem 标量 | Q5 |
| B.5 | compaction | OrioleDB keyspace 下的 image layer 生成正确性验证 | Q3, I3 |

**DoD**：
- [ ] 每个 Q1 事件都有对应 walredo 纯函数，单元测试覆盖
- [ ] walingest summary 可独立重建 = 当前 OrioleDB shmem 状态（差分测试）
- [ ] Layer compaction 在 E2E 用例下不丢不坏
- [ ] I3 chain length empirical 测量，worst-case ≤ Q3 声明上界

### 3.3 Track F — E4 (运行读)

**范围**：
- OrioleDB delta 链上的 GetPage 正确性 empirical 验证
- `wait_lsn` 在密集 OrioleDB 写场景下的行为
- sys-tree 首访延迟测量
- I5-read：branch/PITR 到事务中段的可见性验证

**子任务**：

| # | 内容 | Q 依据 |
|---|---|---|
| F.1 | OrioleDB 事件的 delta chain GetPage 压测（依赖 B 就绪） | Q3, I3 |
| F.2 | sys-tree 首访延迟在 wait_lsn 60s ceiling 内的实测 | I4-B |
| F.3 | Branch 到 mid-txn LSN 的 I5-read 测试场景 | I5-read |
| F.4 | GetPage 超时边界的降级行为审视 | Q3 |

**DoD**：
- [ ] 事件 delta chain 在 empirical workload 下 GetPage 正确
- [ ] sys-tree 首访延迟 < 60s 在目标 workload
- [ ] I5-read 测试：branch 到事务中段快照全可见 xor 全不可见
- [ ] wait_lsn 超时情况有明确降级语义

---

## 4 — Phase 2.5：冷启动路径上线

**门禁：** A + B + F 三条稳态 track 全部 DoD 满足。

### 4.1 Track C — E3 (冷启动)

**范围**：
- basebackup 投递 OrioleDB-state summary 的载体定型
- compute 启动时新增一条"读 summary → 初始化 OrioleDB shmem"的路径
- 老 `orioledb_recovery.signal` 路径**保留未动**（Phase 3 才退）

**子任务**：

| # | 内容 | Q 依据 |
|---|---|---|
| C.1 | basebackup summary 载体选型（建议独立 blob） | Q5 |
| C.2 | basebackup 端生成 summary 流（来自 E2 维护的字段） | Q5 |
| C.3 | compute 端读 summary 初始化 OrioleDB shmem 的新 codepath | Q5, I4-A |
| C.4 | Feature flag 切换新旧路径 | – |
| C.5 | 冷启动一致性测试矩阵（多 checkpoint 后 cold-start） | I4-A |

**DoD**：
- [ ] basebackup 投递 summary 机制上线
- [ ] compute 新 codepath 经 feature flag 启用后能正确启动
- [ ] 冷启动测试矩阵：多 checkpoint 周期后 cold-start 所见 = 预期状态
- [ ] 老 signal 路径保留未动（Phase 3 才退）

---

## 5 — Phase 3：退老路径

**门禁：** Track C 已跑通 + 冷启动矩阵测试通过 + 无 feature-flag 下新路径默认 on 跑够两周无告警。

### 5.1 动刀项

| # | 删除目标 | 文件：行 |
|---|---|---|
| P3.1 | compute 侧 signal + pg_wal 拷贝 | `compute_tools/src/compute.rs:1772-1835` |
| P3.2 | `apply_btree_modify_record` 降级（仅 logical decoding 用） | `pgxn/orioledb/src/recovery/recovery.c:1858` |
| P3.3 | `orioledb_recovery.signal` 生成端删除 | `compute_tools/src/compute.rs` + `pgxn/orioledb/src` signal 生成路径 |
| P3.4 | 保留项（不删）：CONTAINER 发射点 | R14 logical decoding 需要 |

### 5.2 扩展测试

- N8 crash 矩阵加 branch/PITR mid-commit 场景
- 热页面 workload 下 I3 chain length empirical 上界
- sys-tree 首访延迟的长尾分布

---

## 6 — 纪律点

1. **每个 PR 的 commit message 必须陈述对 I1–I5 的影响**（即使是无影响也要写 "no invariant impact"）。这是 CLAUDE.md 的硬性要求，前几轮都被跳过。
2. **Phase 1 不闭合不得进 Phase 2**。历次翻车全部发生在跳过设计收敛。
3. **Phase 2 期间不得改 CONTAINER 编码**。它是 R14 logical decoding 的 source of truth。
4. **Phase 3 前置清单：** A+B+F+C DoD 全满足 + Phase 2.5 稳定运行 ≥ 2 周。
5. **Q-answer 的修改必须回头审 E1–E4 每条 track 对应子项是否仍对齐**。不允许静默修订；要走版本升位 + change log。
6. **Track 内的 PR 顺序由依赖关系决定，不按作者偏好打乱**。
7. **每条 track 的增量都要是 feature-flag 能关掉的形态**；Phase 3 才是拆除老路径。

---

## 7 — 风险登记

| # | 风险 | 触发点 | Fallback |
|---|---|---|---|
| R1 | Q2 审出某事件 redo **不可纯化** | Phase 1 P1.2 | 若 SPLIT/MERGE 不可纯：退化为 PAGE_IMAGE；若 UNDO_APPLY 不可纯：继续用 FPI；若 CONTAINER 不可纯：Phase 3 只降级不删 |
| R2 | Q3 实测链长 **unbounded** | Phase 2 B.5 | 强制 Strategy B（first-write-after-checkpoint 自动 FPI），放弃纯 C（layer compaction）路径 |
| R3 | Layer compaction 在 OrioleDB keyspace **不工作** | Phase 2 B.5 | 放弃 C，采 Strategy B；若 B 实现复杂，MVP 暂用 Strategy A + 缩短 checkpoint 周期 |
| R4 | I5-write 审出 **barrier 顺序违反** | Phase 1 P1.6 | Track A 的 A.6 升级为阻塞级任务；Phase 2 推迟 |
| R5 | wait_lsn 60s ceiling **不够 sys-tree 首访** | Phase 2 F.2 | 调整 `wait_lsn_timeout`；或在 E3 冷启动阶段预加载 sys-tree meta 页 |
| R6 | Q5 发现某 shmem 标量**既非 sys-tree 可得也非 summary 可重建** | Phase 1 P1.5 | 新增一条 walingest summary 字段；若不可能，需回炉改 E1 emit 使其可达 |
| R7 | Phase 3 动刀后 logical decoding 回归 | Phase 3 P3.2 | P3.2 降级不删：`apply_btree_modify_record` 仍编译进 binary，但新 codepath 不调用；logical decoding 独立 consumer 保留 |

---

## 8 — 开放问题（不阻塞 Phase 1 但 Phase 2 前需回答）

- **G4 — page init 覆盖**：SPLIT 之外的 page birth 路径是否全覆盖？需读 `btree/build.c` 初始加载路径。可并入 P1.2（Q2 草案）。
- **Layer compaction 对 sys-tree keys 的表现**：是否与 IOT leaf 一致？可并入 P1.3（Q3）。
- **Branch at `sync_lsn < mid-txn LSN`**：I5-read 在此边界的正确行为。可并入 P1.6 或 F.3。

---

## 9 — 变更日志

- **v1.0 (2026-04-21)** — 初稿。统一 Q（设计问题）× E（实施接口）两维度。Phase 1–3 + 门禁 + DoD + 风险登记。
