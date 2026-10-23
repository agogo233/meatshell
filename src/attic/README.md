# src/attic — 未接线的历史模块（决策④：两阶段清理的归档层）

本目录保存从 `src/` 移出、**不参与任何编译单元** 的上游拆分模块与旧死代码。
rustc 对没有任何 `mod` 引用的 `.rs` 文件完全无视（零告警、零报错），这正是
此前合并上游时"取回文件但不接线"造成功能静默失效的机制。

## 内容与去向

| 文件 | 说明 | 处理 |
| --- | --- | --- |
| `open_window.rs` | 上游窗口构建拆分 | 本地等价实现已在 `app.rs::open_window` |
| `session_callbacks.rs` | 会话列表/对话框回调（对话框测试取消已按最小方案移植进 `app.rs`） | 其余部分待逐项决策 |
| `settings_callbacks.rs` | 设置页 dock 恢复/持久化（本地已有等价实现） | 保留对照 |
| `misc_callbacks.rs` | NIC 选择/下载目录/About（本地已内联等价实现） | 保留对照 |
| `helpers.rs` | clipboard / split_proxy 等（本地已有等价实现） | 保留对照 |
| `drag_out.rs` | #504 拖文件到资源管理器（Windows OLE + macOS AppKit 文件承诺，`drag_out_mac.rs` 为 `#[path]` 兄弟文件） | 决策为暂不实现；上游接线回调 `on_sftp_drag_out` 与桥接 `drag_out_items` 合并时不取回（`sftp_callbacks.rs` / `sftp_ui.rs` 保留本地空壳帧），取用需连同这两处一起接线 |
| `session_test.rs` | `EditorTest`（上游对话框测试票据机制） | 调用方已在 attic；如未来启用需连同 `session_callbacks` 评估 |
| `editor/` | 旧编辑器高亮/括号匹配/语言检测（`main.rs` 的 `mod editor` 已移除，外部零引用） | 与 `app::editor_host` 的活跃实现二选一，勿同时启用 |
| `tests/ui_auth_prompts.rs` | #multi-window 的 prompt 票据/作用域队列测试 | 测的是本地未接线的能力（`*_scoped` 队列与 `abort_test_prompts` 在 auth_dialogs 中不存在）；本批恢复集成测试 CI 时因编译失败归档。**已决定暂存不接线**（2026-10-21）：`app.rs` 已落地的"测试任务完成开关"（commit 20d62b4 的 `ActiveTest::is_finished` 守卫）已覆盖"取消测试不误伤"的核心语义，本文件的"弹窗归属标签"是上游的更彻底方案，两者互斥——接线取回前需先拆掉 `app.rs` 那套，否则同一套取消语义实现两遍、入队口重复注册，编译期即冲突 |

## 复活流程（显式决策）

1. `git mv src/attic/<file>.rs src/app/`（或其它目标位置）
2. 在对应模块树加 `mod` 声明
3. 补齐 `use super::*` 依赖与本地实现的重复符号冲突
4. 跑一次 `cargo test --locked`（本地不编译约定见 AGENTS.md，由 CI 兜底）

直接复制代码回 `src/` 而不删 attic 副本也可以，但**不要**两边同时
`mod` 声明 —— 重复符号与重复回调注册会立即编译失败。
