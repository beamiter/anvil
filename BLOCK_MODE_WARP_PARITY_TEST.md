# jterm1 Block Mode / Warp 体验对齐验收清单

本文用于手工验收 block mode 的块选择、批量操作、命令回填和清空行为。建议按顺序执行；`P0` 项全部通过后，再检查 `P1` 回归项。

## 1. 测试准备

1. 构建并启动一个不恢复旧会话的 block mode 窗口：

   ```bash
   cargo build
   target/debug/jterm1 --mode block --no-restore
   ```

2. 确认当前 shell 已加载 jterm1 shell integration。若尚未配置，可在新窗口的 Bash 中执行：

   ```bash
   source <(target/debug/jterm1 --shell-integration bash)
   ```

3. 依次执行下列命令，准备成功、失败、长输出和后台输出块：

   ```bash
   printf 'alpha\n'
   sh -c 'printf "failed\n" >&2; exit 7'
   printf 'one\ntwo\nthree\n'
   (sleep 1; printf 'background-ready\n') &
   ```

4. 每个普通命令应形成独立块；失败块应显示失败状态；最后一条命令返回提示符后，异步输出应形成无命令的 background block。

## 2. P0：核心验收

### BM-01 命令面板与默认快捷键

- [ ] 按 `Ctrl+Shift+P` 打开命令面板。
- [ ] 搜索 `Select all blocks`，显示快捷键 `Ctrl+Shift+A`。
- [ ] 搜索 `Reinput selected commands`，显示快捷键 `Ctrl+Shift+I`。
- [ ] 搜索 `Clear blocks`，显示快捷键 `Ctrl+Shift+K`。

预期：三个动作均可搜索、可执行，快捷键没有被字体、透明度或 AI 动作覆盖。

### BM-02 全选完成块

- [ ] 在至少有三个完成块时按 `Ctrl+Shift+A`。
- [ ] 所有完成块都出现选中描边。
- [ ] 最新块使用更强的 active 描边，并显示块操作按钮。
- [ ] 按 `Shift+Up`，选择范围从最新端收缩一个块。
- [ ] 按 `Escape`，所有块取消选中，焦点回到输入区后可继续输入。

### BM-03 键盘建立多选范围

- [ ] 按 `Ctrl+Up` 选中最新块。
- [ ] 连续按 `Shift+Up` 扩展到至少三个块。
- [ ] 按普通 `Up` 或 `Down` 后，多选应折叠为新的单个 active 块。
- [ ] 再次建立多选，然后按 `Escape` 清除选择。

预期：浅描边表示整个范围，强描边只表示 active edge；键盘移动时视口能自动带到目标块。

### BM-04 多选命令回填（Enter）

1. 只选择包含命令的两个或三个块。
2. 按 `Enter`。

- [ ] 输入区出现所有所选命令，顺序与终端中从旧到新一致。
- [ ] 多条命令以多行可编辑文本出现。
- [ ] 回填本身不会立即执行命令，也不会生成新块。
- [ ] 回填成功后，块选择被清除，输入区获得焦点。
- [ ] 可编辑任意一行；按 `Ctrl+C` 可安全放弃本次回填。

### BM-05 多选命令回填（快捷键和命令面板）

- [ ] 建立多选后按 `Ctrl+Shift+I`，结果与 BM-04 一致。
- [ ] 建立多选后，从命令面板执行 `Reinput selected commands`，结果与 BM-04 一致。
- [ ] 选区同时包含普通块和 background block 时，回填只包含有命令的块，background block 不产生空行或空命令。

### BM-06 多选右键菜单

1. 建立包含两个以上块的选区。
2. 在选区内任一块上右键。

- [ ] 菜单显示复数动作：`Copy Commands`、`Copy Outputs`、`Copy Blocks`、`Insert Commands at Prompt`。
- [ ] `Copy Commands` 按终端顺序复制所有所选命令，忽略 background block。
- [ ] `Copy Outputs` 按终端顺序复制所有所选输出，块之间保留一个空行。
- [ ] `Copy Blocks` 同时复制每个块的命令和输出，块之间保留一个空行。
- [ ] `Insert Commands at Prompt` 的结果与 BM-04 一致。
- [ ] 在当前选区外的块上右键时，选区先折叠到该块，菜单恢复单数动作。

建议把复制结果粘贴到文本编辑器或临时文档中检查，避免误执行命令。

### BM-07 清空块

1. 创建多个块，并至少完成以下状态中的两项：选中块、添加书签、执行搜索、滚动离开底部让“跳到最新”按钮出现。
2. 按 `Ctrl+Shift+K`。

- [ ] 所有完成块立即消失，当前输入提示符仍可使用。
- [ ] 选区、书签、搜索高亮、虚拟滚动索引和未读徽标一并清除。
- [ ] 按 `Ctrl+Up` 不会重新选中已清除块。
- [ ] 按 `Ctrl+,` / `Ctrl+.` 不会跳到已清除书签。
- [ ] 随后执行 `printf 'after-clear\n'`，新块正常显示、选中、复制和回填。
- [ ] 从命令面板执行 `Clear blocks` 结果相同。

若配置了 `block_history_path`，关闭并重新打开 jterm1 后，被清除的块不应恢复。

### BM-07b 清空撤销（Undo clear blocks）

1. 创建至少三个完成块（含一个失败块），按 `Ctrl+Shift+K` 清空。
2. 观察 toast 提示 `Cleared N blocks — "Undo clear blocks" restores them.`
3. 执行 `printf 'post-clear\n'` 生成一个新块。
4. 从命令面板执行 `Undo clear blocks`。

- [ ] 被清除的块全部恢复，且位于 `post-clear` 新块之上，顺序与清除前一致。
- [ ] toast 显示 `Restored N cleared blocks.`
- [ ] 恢复后的块可以选中、复制、回填、书签，失败块仍显示失败状态。
- [ ] 再次执行 `Undo clear blocks`，toast 显示 `No cleared blocks to restore.`,块列表不变。
- [ ] 在空面板上按 `Ctrl+Shift+K` 不会破坏已有的撤销快照（清空→立即再清空→撤销仍恢复原有块）。
- [ ] 配置了 `block_history_path` 时，撤销后重启,恢复的块仍然存在。
- [ ] 在 alt-screen 程序（如 `less`）内执行 `Undo clear blocks` 不产生效果也不崩溃,退出后再执行可正常恢复。

### BM-07c 失败块跳转

1. 依次制造:成功块、失败块 A、成功块、失败块 B、成功块。
2. 从命令面板执行 `Jump to next failed block` / `Jump to previous failed block`（或绑定 `jump_to_prev_failed` / `jump_to_next_failed` 后用快捷键）。

- [ ] 无选中时,next 跳到最旧失败块,prev 跳到最新失败块。
- [ ] 已选中失败块 A 时,next 跳到失败块 B,prev 在越过最旧失败块后回绕到最新失败块。
- [ ] 目标块被选中并滚动进入视口,长会话（250+ 块）中依然可靠。
- [ ] 无失败块时执行动作无副作用。

### BM-07d 会话导出与 Markdown 复制

- [ ] 从命令面板执行 `Export session as Markdown file`,toast 显示 `Session exported to …/jterm1/exports/session-<时间戳>.md`。
- [ ] 文件包含所有块的命令、输出、退出码,权限为 `0600`。
- [ ] `Export session as JSON file` 同理生成 `.json`,内容为块数组。
- [ ] 同一秒内连续导出两次,第二个文件带 `-1` 后缀,互不覆盖。
- [ ] 多选块后右键选择 `Copy Blocks as Markdown`,剪贴板为按终端顺序拼接的 Markdown,含 `**Exit Code:**` 元数据;单块时菜单项为 `Copy Block as Markdown`。

### BM-08 运行中清空与 Enter 透传

先执行：

```bash
sh -c 'printf "start\n"; sleep 5; printf "done\n"'
```

- [ ] 命令运行期间按 `Ctrl+Shift+K`，旧完成块被清除。
- [ ] 运行中的命令不被 `Ctrl+L`/form-feed 干扰，约 5 秒后仍输出 `done` 并形成新块。

再执行：

```bash
read -r value; printf 'value=%s\n' "$value"
```

- [ ] 命令等待输入时，用鼠标选中一个旧块，再输入 `hello` 并按 `Enter`。
- [ ] `Enter` 被传给运行中的命令，最终输出 `value=hello`；不会错误回填旧块命令。

## 3. P1：复制、兼容性与快捷键回归

### BM-09 现有复制行为

- [ ] 选择单个或多个块后按 `Ctrl+Shift+C`，复制命令和输出。
- [ ] 选择单个或多个块后按 `Ctrl+Alt+Shift+C`，只复制输出。
- [ ] 未选中整块时，在块输出中拖选文本，再按 `Ctrl+Shift+C`，只复制文本选区。
- [ ] 跨块拖选后复制，文本顺序与界面顺序一致。

### BM-10 单块回填与安全降级

- [ ] 单选普通块后按 `Enter`，只回填该块命令且不执行。
- [ ] 单选 background block 后按 `Enter`，不会回填空命令，终端输入仍可继续工作。
- [ ] 在不支持 bracketed paste 的 shell 中，多行回填只保留第一逻辑行，不应意外执行后续行。

### BM-11 统一后的字体与透明度快捷键

- [ ] `Ctrl+=` 能增大字体，`Ctrl+-` 能减小字体。
- [ ] `Ctrl+0` 能把字体缩放复位到 `1.0`。
- [ ] `Ctrl+Alt+-` 能降低窗口透明度。
- [ ] `Ctrl+Alt+=` 能提高窗口透明度。
- [ ] `Ctrl+Alt+Shift+A` 能打开 Session AI panel。
- [ ] `Ctrl+Shift+L` 仍聚焦 tab filter，不会清空块。

### BM-12 VTE 模式与全屏程序

1. 启动普通 VTE 模式：

   ```bash
   target/debug/jterm1 --mode vte --no-restore
   ```

- [ ] 执行普通命令、复制粘贴和滚动均无回归。
- [ ] 执行 block-only 动作不会崩溃或产生伪块。

2. 回到 block mode，运行 `less README.md`、`vim`、`top` 或其他 alt-screen 程序。

- [ ] 程序进入和退出时，历史块隐藏/恢复正常。
- [ ] `Ctrl+Shift+A` 和 `Ctrl+Shift+I` 在 alt-screen 中不会错误选中或回填隐藏块。
- [ ] 退出全屏程序后，块选择、回填和清空仍可继续使用。

### BM-13 长会话与清空后的虚拟滚动

生成至少 250 个小块：

```bash
for i in $(seq 1 250); do printf 'block-%03d\n' "$i"; done
```

若 shell integration 把整个循环视为一个块，请改为手工/脚本逐条提交，或恢复已有长会话进行测试。

- [ ] `Home` / `End`、`PageUp` / `PageDown` 可正常浏览。
- [ ] `Ctrl+Shift+A` 能选中全部块，无明显卡死。
- [ ] `Ctrl+Shift+K` 清空后没有大段空白、残留滚动范围或不可见的新块。
- [ ] 清空后连续执行 10 条命令，每条都正常出现。

## 4. 问题记录模板

```text
用例编号：BM-__
结果：通过 / 失败
桌面环境：X11 / Wayland，桌面或窗口管理器版本
Shell：bash / zsh / fish / pwsh 及版本
jterm1 commit：git rev-parse --short HEAD
是否加载 shell integration：是 / 否
复现步骤：
实际结果：
期望结果：
日志或截图：
```

发现问题时，建议同时附上：

```bash
target/debug/jterm1 --doctor
RUST_LOG=jterm1=debug target/debug/jterm1 --mode block --no-restore
```
