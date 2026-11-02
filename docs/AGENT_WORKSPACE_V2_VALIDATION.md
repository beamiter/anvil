# Agent Workspace V2 发布前验证记录

日期：2026-11-02。本文记录 Anvil 的实际验证范围；其他前端和 core 的测试结果分别记录，未把编译成功计作测试通过。

## 构建与环境

- Linux，Rust 1.99.1；GTK 4.14.5、libadwaita 1.5.0、VTE 0.76.0。
- `cargo check`、开发 profile 的应用二进制构建和测试目标构建通过。开发验证使用单个构建 job、关闭调试信息；部分主包使用仅命令行指定的优化级别覆盖，没有修改 release profile。
- 本轮功能测试与窗口检查使用本地 `jterm_core` patch 构建；最终 Git pin 的锁定依赖编译结果见末节。临时 path patch 不进入提交。
- 原生窗口使用 X11/Xvfb，实际绘制画布为 1600×1100。测试以 UID/GID 65534、无附加组运行，独立临时 HOME/XDG 目录和合成 Workspace 状态，不读取真实 provider 记录或凭据。
- 未修改生产环境中的 helper 信任策略。普通用户测试避免 root 对系统 helper 拥有写权限时触发已有保护。

## 通过的测试

| Gate | 结果 | 验证内容 |
| --- | --- | --- |
| `workspace_` 筛选 | 80 passed，0 failed，25 ignored | 共享 Workspace reducer、身份路由、读取响应、上下文、草稿和既有相关逻辑 |
| `workspace_v2_native_identity_attention_and_narrow_inspector` | 1 passed | 稳定会话身份、交互期顺序、分页外 Needs you、删除不重定向、窄屏 inspector |
| `workspace_v2_composer_has_three_lines_and_visual_caret_space` | 1 passed | 三行编辑区和光标下一行视觉留白；留白不加入文本 |
| `workspace_v2_handoff_prefills_an_editable_unsent_task` | 1 passed | 交接目标为可编辑、未提交的新任务，窄屏提交按钮可见 |
| `workspace_v2_api_composer_keeps_portrait_width_after_render` | 1 passed | API 面板在实际 360×640 与 55 px 左侧 rail 下不溢出 |
| `workspace_handoff_creation_receipt_is_inactive` | 1 passed | 精确新建回执不切换原会话，不覆盖原输入 |

合计 **85 个通过的测试**。后五项为分别在独立进程中执行的 ignored fixture；没有把它们在普通筛选中的 ignored 状态另计为通过，也没有累计重复调试运行。`workspace_` gate 中另外 2207 个测试被过滤，未宣称全量测试通过。

测试先构建相应的 test harness，再在隔离环境中运行：

```sh
<test-binary> workspace_ --test-threads=1 --nocapture
<test-binary> <native-fixture-name> --ignored --test-threads=1 --nocapture
```

原生 fixture 设置 `DISPLAY`、`GDK_BACKEND=x11`、`GSK_RENDERER=cairo`、独立 `JTERM_WORKSPACE_STATE_DIR`；需要 host 的 fixture 用 `JTERM_TEST_HOST` 指向本次构建的 executable。每个 fixture 使用独立临时目录和进程，不复用用户会话。

## 实际窗口检查

| 场景 | 实际结果 |
| --- | --- |
| 540×960 和 360×640 窗口 | X 窗口几何尺寸得到确认；草稿、输入区与底部操作保持在窗口中，默认 inspector 收起 |
| Workspace 折叠与恢复 | 侧栏释放空间；当前会话和未发送草稿保留 |
| Anvil → Forge 草稿 | 关闭 Anvil 后用独立应用配置打开 Forge，共享三行草稿和界面编辑内容保留 |
| Anvil → Forge Context | 保存的 revision 和材料来源在另一端正确读回 |
| 中文正文搜索 | 只存在于合成正文的中文词可命中；打开带确切来源/revision 的只读证据 |
| 深色证据与 Context | 搜索、材料和证据边缘区域与深色界面一致；窄屏不裁掉右侧内容 |
| Created task 与队列 | Anvil 通过真实 Create 流程建立独立 Git worktree；保存 Context 和 Local 队列意图。Forge 能读取其共享材料与队列，缺少当前 task facts 时保持保守状态 |
| 无隐式执行 | 所检查流程完成后，对隔离状态的只读审计确认 `api_jobs=0`、`native_jobs=0`；没有调用模型、点击 Start 或运行 Validate |

实际检查补齐了 API 工具栏和隐藏搜索栏的最小宽度、任务表单的滚动与固定页脚、深色证据边缘，以及新建任务目录 ACK 后的 Context/Queue 路径。相关修复后的应用重新构建并检查了窗口。

## 边界

这些结果没有覆盖 Wayland、真实 IME 候选/组合输入、屏幕阅读器、真实 provider 或付费模型、真实远程 SSH 主机、DLC 执行，以及运行中模型请求期间关闭并恢复 GUI。输入检查使用直接 Unicode 和按键，不能当作 IME 验收。窗口截图来自开发 profile，不能当作 release 打包或性能基准。

共享 core 的外部任务缺少当前状态时应显示 `Task details unavailable`，其最终修复和测试由 core 的验证记录说明。队列和操作权限始终依据真实能力，不根据标签开放执行。

## 发布依赖检查

最终主应用和 `scripts/link-recovery` 的 `Cargo.toml` 与 `Cargo.lock` 均固定到共享 core 提交
`d7387fce27bf2b1e60ff2652ab04239a0b809dd3`。应用依赖与测试依赖使用同一个 revision，锁文件保留 Git source，没有 path patch。

| 最终依赖检查 | 结果 |
| --- | --- |
| 主应用 `cargo check --offline --locked --tests` | 通过，退出码 0，55.81 秒 |
| `scripts/link-recovery` 中 `cargo check --offline --locked` | 通过，退出码 0，103.99 秒 |

使用 Rust 1.99.1、单构建 job、关闭开发调试信息；没有重复计入上述已通过的测试数量。辅助工具锁文件只增加新 core 所需的 SHA-256 依赖，并替换 core revision，没有升级其原有包版本。首次离线解析缺少已锁定的缓存包，按原锁文件只读下载后，最终检查在离线和锁定依赖条件下通过。

此次 Git source 缓存由精确的本地 core 提交准备，检查没有使用 Cargo path override。这确认了锁定提交的编译一致性，**不代表 GitHub 已包含该提交**。远端发布尚待完成，应先发布共享 core，再发布依赖该 revision 的前端。
