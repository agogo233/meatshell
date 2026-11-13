# src/attic — 未接线的历史模块（决策④：两阶段清理的归档层）

本目录保存从 `src/` 移出、**不参与任何编译单元** 的上游拆分模块与旧死代码。
rustc 对没有任何 `mod` 引用的 `.rs` 文件完全无视（零告警、零报错），这正是
此前合并上游时"取回文件但不接线"造成功能静默失效的机制。

2026-11 清理：`settings_callbacks.rs`（7 个函数已内联 `open_window`）、
`misc_callbacks.rs`（`wire_downloads_and_links`/`set_about_libraries` 本地已有同名
实现）为过时副本，已删除；`open_window.rs` 已由 `src/app/open_window.rs` 启用。

## 内容与去向

| 文件 | 说明 | 处理 |
| --- | --- | --- |
| `drag_out.rs` | #504 拖文件到资源管理器（Windows OLE + macOS AppKit 文件承诺，`drag_out_mac.rs` 为 `#[path]` 兄弟文件） | 保留暂缓（2026-11 复核）：UI 层无 DragArea、无调用方（`ui/terminal_view.slint` 注释"暂不实现"），启用需新增 Slint 拖出手势 + Rust 侧接线 `drag_out::start`，属跨平台新功能开发；取用需连同 `sftp_callbacks.rs` / `sftp_ui.rs` 一起接线 |
| ~~`session_test.rs`~~ | `EditorTest`（上游对话框测试票据机制） | 已启用（2026-11）：移回 `src/app/session_test.rs`，替换 session_callbacks 的 ActiveTest 为 EditorTest/TestTicket 票据 |
| `editor/` | 旧编辑器高亮/括号匹配/语言检测（`main.rs` 的 `mod editor` 已移除，外部零引用） | 与 `app::editor_host` 的活跃实现二选一，勿同时启用。括号匹配与跨行着色（heredoc / YAML 块标量）已由活跃的 `app::editor_syntax` 重新实现（`bracket_pair` / `spans_for`），本目录副本仅供对照 |
| `tests/ui_auth_prompts.rs` | #multi-window 的 prompt 票据/作用域队列测试 | 测的是本地未接线的能力（`*_scoped` 队列与 `abort_test_prompts` 在 auth_dialogs 中不存在）；本批恢复集成测试 CI 时因编译失败归档。**已决定暂存不接线**（2026-10-21）：`app.rs` 已落地的"测试任务完成开关"（原 `ActiveTest::is_finished` 守卫，2026-11 已升级为上游 `EditorTest` 票据，见 session_test）已覆盖"取消测试不误伤"的核心语义；本文件的"弹窗归属标签"是上游的更彻底方案，两者互斥——接线取回前需先拆掉现 `EditorTest` 套，否则同一套取消语义实现两遍、入队口重复注册，编译期即冲突 |

## 复活流程（显式决策）

1. `git mv src/attic/<file>.rs src/app/`（或其它目标位置）
2. 在对应模块树加 `mod` 声明
3. 补齐 `use super::*` 依赖与本地实现的重复符号冲突
4. 跑一次 `cargo test --locked`（本地不编译约定见 AGENTS.md，由 CI 兜底）

直接复制代码回 `src/` 而不删 attic 副本也可以，但**不要**两边同时
`mod` 声明 —— 重复符号与重复回调注册会立即编译失败。
