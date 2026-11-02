# Agent Workspace

anvil 默认把会话导航放在左侧，中央显示选中的真实终端、API 对话或共享 host 托管的 Agent 任务。普通终端仍在侧栏底部的 **Terminals** 中；顶部不再并排放会话标签。

## 使用

左侧 **Conversations** 同时列出本机 Codex / Claude CLI 历史、共享 API 对话和任务记录。搜索、项目筛选、Pinned / Recent、Needs you / Running 作用于同一列表。单击标题或在搜索栏用方向键选中后按 Enter，打开明确选中的会话；仅移动选择不会启动进程或发送提示。会话菜单提供 Pin / Unpin；API 和 CLI 会话还可以归档、恢复。CLI 的名称和标签可在侧栏更多菜单编辑。

**＋ New** 提供 **New API chat / New task / New terminal**，以及独立的 **Remote workspace · read only** 入口。API 对话和原生任务显示在中央工作区；终端输入继续进入原来的 PTY。原生 API / Agent 编辑器支持多行：**Enter 换行，Ctrl+Enter 发送**，输入法先处理候选字。右侧检查器可单独打开，不占用聊天主视图。

## 三类会话的运行边界

| 会话 | 打开方式 | 关闭窗口以后 |
| --- | --- | --- |
| 已有 Codex / Claude CLI | 校验历史来源、项目目录和确切 provider session ID；已有本窗口终端直接聚焦，否则连接共享 host 拥有的 PTY | 已托管的 CLI 继续运行；**Ctrl+]** 从终端断开后可以在另一端重新连接 |
| API 对话 | 四端读取同一共享存储；host 持提交凭据执行已接受的请求 | 已接受的 HTTP 工作由 host 持有；关闭视图不重发，也不等同于 Stop。消息、草稿、阅读位置保留 |
| 原生 Agent task | 复用 host 中的真实 TaskManager、隔离工作区、事件流和审阅流程 | 原生运行和验证由 host 持有；其他窗口读取同一快照，执行控制仍需当前 run、sequence 和所有权检查 |

一个 CLI 会话只允许一个活动输入连接；第二个写入连接会收到明确的占用说明。按 Ctrl+] 是断开连接，不是向 provider 发送快捷键。新建普通终端后自行启动的 CLI，尚未获得可验证 provider session ID 时仍按普通终端生命周期处理；已有历史会话才可以通过侧栏确切恢复并托管。

运行状态来自当前进程、host 租约和原生快照。共享 projection 区分运行状态、任务状态、验证与 Needs you 原因；旧 CLI 历史中的 Waiting 文本本身不构成实时审批。第三方 TUI 的内部状态不能完整实时解析，未确认状态明确显示。

## API 历史和草稿

新的 API 对话由共享存储分配全局身份。现有窗口快照中的旧聊天只在明确的迁移边界导入；不同应用、窗口和 provider home 不会靠相同的数字编号或 UUID 猜测为同一会话。

发送前，前端先保存当前草稿，再向 host 申请绑定会话身份、版本和 epoch 的提交凭据。只有凭据成功后才启动 HTTP 请求。完成、停止和删除都等待对应确认；过期回调不能写入已切换的会话。两个窗口同时发送到同一对话时，后来的请求会被拒绝并保留草稿。

响应期间的新编辑不会因为回复完成被清空。如果另一窗口同时修改草稿，前端采用明确的草稿版本比较：能证明没有本地更改时加载远端草稿；双方都改动时保留本地内容并要求显式 **Reload shared history** 后再发送，不把新收到的版本号当成覆盖权限。Reload 明确保留当前本地草稿；**Earlier messages** 分页读取更早消息，不把额外浏览历史自动注入 provider 上下文。

共享保存继续遵守 `ai_redact_secrets`：普通快照、提交文本和最终回答都走已有脱敏策略。完成回答先经过 ChatStore 的长度限制，再提交共享追加。聊天缓存有界；更早消息存于 SQLite，按需分页载入。普通终端窗口快照仍保留为本地恢复入口。

## 原生 provider 能力

Codex 与 Claude 使用已有原生事件投影显示实际文本、命令、文件更改和状态，不展示生产演示数据。Codex 的干净 **Finish** 完成后，在 host 仍保有受控 profile 和确切线程身份时提供 **Resume native**；恢复后读取最近历史，**Recent history / Load earlier** 使用 provider 历史接口分页。历史是只读文本，不会重放过去工具或批准动作。

Claude 原生任务可以多轮继续；每轮使用返回的确切 session ID，并在启动下一轮前确认上一轮进程结束。需要完整交互权限对话或 TUI 专有能力时使用 Terminal。Claude 的原生权限拒绝作为可操作错误显示，不自动跳过权限。Kimi 保留单轮能力；OpenCode 使用现有 Terminal 路径。

新建的本地任务先等待 host 确认来源身份，再开放 Context 与队列；登记过程中保留各会话草稿，不提前发送读写请求。共享任务事实同步完成后，其他窗口可进入同一任务的真实控制页；同步前先显示只读详情，不从目录元数据推造 native resume 或启动权限。现有失败后的 Terminal fallback 会开启新的 CLI 目标流程；它不宣称与原生 provider 线程具有同一完整历史。

`agent_tasks_enabled` 在普通启动配置中默认启用，显式 `false` 保持关闭；safe mode 继续关闭发送/Agent 动作。`ai_share_command_context` 仍默认 `false`，任务发送上下文需要用户已有设置允许。

## 共享架构

GTK 线程只排队命令并轮询结果，不阻塞等待扫描、数据库、provider 或 host 启动。`jterm_core::agent_workspace` 提供共享 SQLite 存储、source-qualified catalog、提交凭据、后台 API / 原生任务 / PTY host 和连接客户端；应用在 GUI 初始化前分派内部 host / attach 入口。两个 GTK 前端复用既有 VTE、IME、Enter-release 保护以及任务审批/完成确认。

CLI 组织信息由共享数据库按 provider、主机、provider home 和 native ID 保存。旧 SessionLibrary 只作为首次迁移提示；出现多个 provider home 的同 UUID 时不猜测迁移归属。任务索引还包含应用/配置命名空间；共享置顶只改变导航元数据，不改变 task runtime 所有权。

本工作区整合本地 CLI 历史和应用自己的 API 对话。它没有读取 ChatGPT 或 Claude 消费者桌面/网页账户的私有聊天列表，也不复刻它们的专有功能。


## 稳定导航与共享查询

列表以 source-qualified conversation ID 保持 GTK 行实例。流式状态更新只改文字；鼠标停留、按键浏览、菜单展开或滚动阅读期间保持已有顺序，出现新活动时可用 **Update conversation order** 明确重排。首次显示 120 行，**Show more** 继续展开。**Needs you** 循环定位当前筛选结果中的待处理会话，包括尚未显示的行；打开会话后仍需使用原有明确操作来回复、审批或重试。

目录与历史共用查询语法，例如 `provider:codex attention:approval`、`title:"数据库迁移" -is:archived`、`validation:failed`、`body:"具体错误"`。未知字段或不完整引号会显示错误并停止匹配。侧栏搜索同时显示目录结果与 **History matches**；历史搜索有 180 ms 防抖与分页，查询版本变化后丢弃旧响应。

历史命中先打开只读证据，按 conversation、event ID、revision 精确读取。过期或删除的材料报错，不静默替换为新版本。**Open conversation** 是另一个明确动作。每个会话菜单的 **Conversation details → Search history** 只搜索该会话。覆盖提示区分已索引文档、等待索引的消息和 provider 历史范围；CLI 目录扫描提供元数据，不宣称扫描了全部 provider 对话全文。

## Context、Changes 与 Run

会话详情用原生下拉页选择器提供 **Details / Context / Search history**，任务另有 **Changes / Run**，窄窗无需横向挤出五个标签。Context 包含目标、已完成、剩余、约束与最多 16 份选定材料，总预算 64 KiB。可加入可编辑笔记/粘贴片段、精确搜索证据，以及捕获的 diff / 验证日志；每份材料都可预览、修改和移除。读取历史不会自动把文本加入模型上下文。

**Save materials** 用 revision 比较保存。另一窗口已保存新版本时保留本地草稿并要求选择保留本地或使用共享版本；保存回执到达时继续保留保存期间的新编辑。切换会话保留其内存草稿，并在替换上下文前重置旧编辑器的 IME。未保存的 Context 编辑不保证跨进程恢复。

选择 Codex / Claude / Kimi / API 目标并点击 **Prepare handoff preview** 后，先显示完整审阅文本；**Open editable new draft** 才预填目标。任务使用现有新建表单，API 使用新建回执绑定的空草稿。API 新建等待期间若用户切换会话、继续输入、开始 IME 组合或移走焦点，回执只完成侧栏中的目标草稿，不抢回当前输入；已有非空目标草稿不会被覆盖。两者都不自动发送、启动或把源线程恢复成另一个 provider。超出 16 KiB 目标任务预算会明确报错并保留源材料。

**Changes** 由后台只读 worker 捕获基线、HEAD、分支、状态、diff、未跟踪文件数量与指纹；显示捕获时间和来源 sequence。diff 包含相对任务基线的全部修改，不宣称全由 Agent 产生。**Run** 显示验证命令、目录、shell、时间、日志与指纹绑定状态；当前 source sequence 改变时提示刷新。读取不会自动运行验证或接受结果。provider 实际报告的 tokens / cost 附来源范围展示；缺失时显示不可用，不估算为零。

Run 页的共享依赖队列可加入、移除和选择任务依赖；写入用队列 revision 比较，循环依赖由 host 拒绝。Ready 只表示可进入现有任务控制页，必须显式 Start；验证通过不等于依赖已验收。当前执行位置仅提供 Local，未验证的 SSH / DLC 适配器不会显示为可启动功能。侧栏资源概览显示观察到的 CLI / API / native / 验证数量、已验收和未确认任务；目录未完整加载时明确标注覆盖限制。

Workspace toggle 只折叠导航内容，保留原任务、草稿、布局与后台工作。中央视图保持连续左对齐，右检查器可收起；原生文本输入保留至少三行和光标下一行留白；留白是原生布局 margin，不进入发送文本。New task 表单可滚动，Cancel / Create task 固定留在可视底部。

## 远程只读工作区

**＋ New → Remote workspace · read only** 打开独立连接窗口。填写已有 SSH alias、远端已安装本版本终端的绝对路径与预期 host ID；在远端用 `<terminal executable> --jterm-workspace-host-id` 取得身份。连接使用严格 known_hosts、BatchMode 与协议/host ID 检查，只读取已经运行的远端 workspace host，不自动启动 host 或 provider。

远程目录可分页，点击会话搜索该会话已索引历史，搜索命中按确切 revision 读取。重连或修改查询后丢弃旧响应和旧控件事件。该窗口没有本地激活、发送、Context 保存或队列写入入口；SSH 执行队列和 DLC 启动仍需独立已验证适配器。开发验收应使用本地 SSH 协议 fixture；真实服务器需要独立的连接验收。

## 协议升级

本批共享 host 与四前端使用协议版本 2，必须同步更新 core pin。遇到旧 host 协议不兼容会报告错误，不自动杀进程、抢写入权或重放提交。先处理原 host 的工作，再按明确操作重启相应会话。

## 开发验证

构建后可用原生测试验证侧栏按真实身份激活、键盘浏览不启动和主窗口不会被旧 modal 关闭逻辑关闭：

```bash
cargo build --bin anvil
xvfb-run -a cargo test embedded_workspace_navigation_focuses_exact_session_without_closing_window -- --ignored --test-threads=1
JTERM_TEST_HOST="$PWD/target/debug/anvil" cargo test shared_receipts_reject_second_writer_preserve_new_draft_and_delete_after_cancel -- --ignored --test-threads=1
```

第二个测试启动独立临时状态目录中的真实 embedded host，验证两个客户端的提交排他、响应与草稿保存交错、阅读位置、取消后删除；不连接真实 provider。GUI 发送/IME和任务实际运行还应在桌面环境验证。

新的原生导航回归需要隔离状态目录：

```bash
JTERM_WORKSPACE_STATE_DIR="$(mktemp -d)" xvfb-run -a cargo test workspace_v2_native_identity_attention_and_narrow_inspector -- --ignored --test-threads=1
cargo test workspace_ -- --test-threads=1
```

这些测试验证稳定身份、交互期排序、分页外 Needs you 定位、删除不替换选中目标、精确证据响应与上下文版本冲突；`workspace_v2_api_composer_keeps_portrait_width_after_render` 检查真实 API 面板在 360×640 与左侧 55 px rail 下不溢出，handoff 测试检查该尺寸下的任务提交按钮仍可见且预填没有提交。`workspace_handoff_creation_receipt_is_inactive` 使用 `JTERM_TEST_HOST` 指定的本地 host 验证新建回执不切换原会话、不覆盖原输入。每个 ignored GTK 测试应在独立进程中运行。原生显示测试不代表真实 provider、桌面输入法或断电恢复的端到端认证。

本次实际执行的测试数量、原生窗口检查及未覆盖范围见 [V2 发布前验证记录](AGENT_WORKSPACE_V2_VALIDATION.md)。
