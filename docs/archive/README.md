# Archived Plans

Historical planning documents, superseded by current architecture.

| File | Superseded by | Reason |
|---|---|---|
| `PAGE_LEVEL_WAL_PLAN_v1_2026-04-16.md` | `../LOG_IS_DATA_ARCHITECTURE.md` | 过于严格：声称 "per-mutation block-keyed eager WAL" 是目标，导致推演出过高的 WAL 体积代价。v2.1 修正为 **commit-barrier** 模式：只保证 commit 涉及的关键 block 已入 WAL，非关键 block 继续 lazy flush。v1 的"Phase 1-5 Complete"也是过度乐观——实际 Phase 5 cleanup 从未做到位。 |

归档文件仅作历史参考。所有新计划 / 修改对齐 `docs/LOG_IS_DATA_ARCHITECTURE.md`。
