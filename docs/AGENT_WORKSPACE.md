# Agent Workspace

anvil 默认把会话导航放在左侧，中央显示选中的真实终端、API 对话或本窗口拥有的 Agent 任务。普通终端仍在侧栏底部的 **Terminals** 中；顶部不再并排放会话标签。

## 使用

左侧 **Conversations** 同时列出本机 Codex / Claude CLI 历史、共享 API 对话和任务记录。搜索、项目筛选、Pinned / Recent、Needs you / Running 作用于同一列表。单击标题或在搜索栏用方向键选中后按 Enter，打开明确选中的会话；仅移动选择不会启动进程或发送提示。会话菜单提供 Pin / Unpin；API 和 CLI 会话还可以归档、恢复。CLI 的名称和标签可在侧栏更多菜单编辑。

**New conversation** 提供 AI conversation、Agent task、Terminal 三个真实入口。API 对话和原生任务显示在中央工作区；终端输入继续进入原来的 PTY。原生 API / Agent 编辑器支持多行：**Enter 换行，Ctrl+Enter 发送**，输入法先处理候选字。右侧检查器可单独打开，不占用聊天主视图。

## 三类会话的运行边界

| 会话 | 打开方式 | 关闭窗口以后 |
| --- | --- | --- |
| 已有 Codex / Claude CLI | 校验历史来源、项目目录和确切 provider session ID；已有本窗口终端直接聚焦，否则连接共享 host 拥有的 PTY | 已托管的 CLI 继续运行；**Ctrl+]** 从终端断开后可以在另一端重新连接 |
| API 对话 | 四端读取同一共享存储；使用各端现有 provider 和凭据设置 | 已完成消息、草稿和阅读位置保留；窗口关闭取消其 HTTP 请求，不把它伪装成后台 Agent |
| 本窗口原生 Agent task | 复用真实 TaskManager、隔离工作区、事件流和审阅流程 | 原生 task runtime 仍由窗口拥有，随窗口关闭；共享列表中的任务记录只保留项目/上下文，不授予另一窗口运行权 |

一个 CLI 会话只允许一个活动输入连接；第二个写入连接会收到明确的占用说明。按 Ctrl+] 是断开连接，不是向 provider 发送快捷键。新建普通终端后自行启动的 CLI，尚未获得可验证 provider session ID 时仍按普通终端生命周期处理；已有历史会话才可以通过侧栏确切恢复并托管。

运行状态来自当前进程、host 租约或本窗口 runtime。CLI 的“需要输入”仍结合 provider 最近保存事件和当前进程证据；它不是对每个第三方 TUI 内部状态的完整实时解析。

## API 历史和草稿

新的 API 对话由共享存储分配全局身份。现有窗口快照中的旧聊天只在明确的迁移边界导入；不同应用、窗口和 provider home 不会靠相同的数字编号或 UUID 猜测为同一会话。

发送前，前端先保存当前草稿，再向 host 申请绑定会话身份、版本和 epoch 的提交凭据。只有凭据成功后才启动 HTTP 请求。完成、停止和删除都等待对应确认；过期回调不能写入已切换的会话。两个窗口同时发送到同一对话时，后来的请求会被拒绝并保留草稿。

响应期间的新编辑不会因为回复完成被清空。如果另一窗口同时修改草稿，前端采用明确的草稿版本比较：能证明没有本地更改时加载远端草稿；双方都改动时保留本地内容并要求显式 **Reload shared history** 后再发送，不把新收到的版本号当成覆盖权限。Reload 明确保留当前本地草稿；**Earlier messages** 分页读取更早消息，不把额外浏览历史自动注入 provider 上下文。

共享保存继续遵守 `ai_redact_secrets`：普通快照、提交文本和最终回答都走已有脱敏策略。完成回答先经过 ChatStore 的长度限制，再提交共享追加。聊天缓存有界；更早消息存于 SQLite，按需分页载入。普通终端窗口快照仍保留为本地恢复入口。

## 原生 provider 能力

Codex 与 Claude 使用已有原生事件投影显示实际文本、命令、文件更改和状态，不展示生产演示数据。Codex 的干净 **Finish** 完成后，在同一应用进程仍保有受控 profile 和确切线程身份时提供 **Resume native**；恢复后读取最近历史，**Recent history / Load earlier** 使用 provider 历史接口分页。历史是只读文本，不会重放过去工具或批准动作。

Claude 原生任务可以多轮继续；每轮使用返回的确切 session ID，并在启动下一轮前确认上一轮进程结束。需要完整交互权限对话或 TUI 专有能力时使用 Terminal。Claude 的原生权限拒绝作为可操作错误显示，不自动跳过权限。Kimi 保留单轮能力；OpenCode 使用现有 Terminal 路径。

另一个窗口的任务行在中央打开只读项目/上下文页，不能冒充本窗口的 native resume。现有失败后的 Terminal fallback 会开启新的 CLI 目标流程；它不宣称与原生 provider 线程具有同一完整历史。

`agent_tasks_enabled` 在普通启动配置中默认启用，显式 `false` 保持关闭；safe mode 继续关闭发送/Agent 动作。`ai_share_command_context` 仍默认 `false`，任务发送上下文需要用户已有设置允许。

## 共享架构

GTK 线程只排队命令并轮询结果，不阻塞等待扫描、数据库、provider 或 host 启动。`jterm_core::agent_workspace` 提供共享 SQLite 存储、source-qualified catalog、提交凭据、PTY host 和连接客户端；应用在 GUI 初始化前分派内部 host / attach 入口。两个 GTK 前端复用既有 VTE、IME、Enter-release 保护以及任务审批/完成确认。

CLI 组织信息由共享数据库按 provider、主机、provider home 和 native ID 保存。旧 SessionLibrary 只作为首次迁移提示；出现多个 provider home 的同 UUID 时不猜测迁移归属。任务索引还包含应用/配置命名空间；共享置顶只改变导航元数据，不改变 task runtime 所有权。

本工作区整合本地 CLI 历史和应用自己的 API 对话。它没有读取 ChatGPT 或 Claude 消费者桌面/网页账户的私有聊天列表，也不复刻它们的专有功能。

## 开发验证

构建后可用原生测试验证侧栏按真实身份激活、键盘浏览不启动和主窗口不会被旧 modal 关闭逻辑关闭：

```bash
cargo build --bin anvil
xvfb-run -a cargo test embedded_workspace_navigation_focuses_exact_session_without_closing_window -- --ignored --test-threads=1
JTERM_TEST_HOST="$PWD/target/debug/anvil" cargo test shared_receipts_reject_second_writer_preserve_new_draft_and_delete_after_cancel -- --ignored --test-threads=1
```

第二个测试启动独立临时状态目录中的真实 embedded host，验证两个客户端的提交排他、响应与草稿保存交错、阅读位置、取消后删除；不连接真实 provider。GUI 发送/IME和任务实际运行还应在桌面环境验证。
