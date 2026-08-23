# anvil Block Mode / Warp 体验对齐验收清单

本文用于手工验收 block mode 的块选择、批量操作、命令回填和清空行为。建议按顺序执行；`P0` 项全部通过后，再检查 `P1` 回归项。

## 1. 测试准备

1. 构建并启动一个不恢复旧会话的 block mode 窗口：

   ```bash
   cargo build
   target/debug/anvil --mode block --no-restore
   ```

2. 确认当前 shell 已加载 anvil shell integration。若尚未配置，可在新窗口的 Bash 中执行：

   ```bash
   source <(target/debug/anvil --shell-integration bash)
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

若配置了 `block_history_path`，关闭并重新打开 anvil 后，被清除的块不应恢复。

### BM-07b 清空撤销（Undo clear blocks）

1. 创建至少三个完成块（含一个失败块），按 `Ctrl+Shift+K` 清空。
2. 观察 toast 提示 `Cleared N blocks.`,右侧带 `Undo` 按钮。
3. 执行 `printf 'post-clear\n'` 生成一个新块。
4. 从命令面板执行 `Undo clear blocks`。

- [ ] 被清除的块全部恢复，且位于 `post-clear` 新块之上，顺序与清除前一致。
- [ ] toast 显示 `Restored N cleared blocks.`
- [ ] 恢复后的块可以选中、复制、回填、书签，失败块仍显示失败状态。
- [ ] 再次执行 `Undo clear blocks`，toast 显示 `No cleared blocks to restore.`,块列表不变。
- [ ] 在空面板上按 `Ctrl+Shift+K` 不会破坏已有的撤销快照（清空→立即再清空→撤销仍恢复原有块）。
- [ ] 配置了 `block_history_path` 时，撤销后重启,恢复的块仍然存在。
- [ ] 在 alt-screen 程序（如 `less`）内执行 `Undo clear blocks` 不产生效果也不崩溃,退出后再执行可正常恢复。
- [ ] 直接点 toast 上的 `Undo` 按钮，效果与命令面板一致。
- [ ] 在 A 面板清空后立刻切到 B 面板（或另一个 tab）再点 `Undo`：恢复的是 A
      面板的块，B 面板不受影响。
- [ ] 清空后关闭该面板再点 `Undo`：toast 提示面板已关闭，不崩溃。

### BM-07c 失败块跳转

1. 依次制造:成功块、失败块 A、成功块、失败块 B、成功块。
2. 从命令面板执行 `Jump to next failed block` / `Jump to previous failed block`（或绑定 `jump_to_prev_failed` / `jump_to_next_failed` 后用快捷键）。

- [ ] 无选中时,next 跳到最旧失败块,prev 跳到最新失败块。
- [ ] 已选中失败块 A 时,next 跳到失败块 B,prev 在越过最旧失败块后回绕到最新失败块。
- [ ] 目标块被选中并滚动进入视口,长会话（250+ 块）中依然可靠。
- [ ] 无失败块时执行动作无副作用。

### BM-07d 会话导出与 Markdown 复制

- [ ] 从命令面板执行 `Export session as Markdown file`,toast 显示 `Session exported to …/anvil/exports/session-<时间戳>.md`。
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

### BM-08b 运行中的 live 卡片跟着输出长高（无闪屏）

- [ ] 在已有至少一个完成块的情况下执行 `sleep 3; ls`。
- [ ] 命令运行期间，live 卡片保持提示符大小（约 6 行），上方的完成块**始终可见**：
      不允许出现"整块占满全屏、结束后再恢复排列"的闪烁。
- [ ] 底部状态栏的网格尺寸在运行期间仍是整个视口的行数（例如 `137x49`），
      而不是卡片显示的 6 行 —— 卡片被裁剪，终端网格没有变小。
- [ ] 执行 `for i in 1 2 3 4 5 6 7 8; do echo line-$i; sleep 0.4; done`：
      卡片每来一行长高一行，历史平滑上移，不跳动。
- [ ] 执行 `clear; tput cup 18 4; echo DEEP; tput cup 2 0; echo TOP; sleep 3`：
      `TOP` 与 `DEEP` 落在各自的绝对行号上，一行都不能丢
      （清屏会让 live 卡片退回整页高度，这是预期的安全回退）。
- [ ] 执行 `tput smcup; tput cup 3 3; echo IN-ALT; sleep 2; tput rmcup`：
      alt-screen 期间 live 表面占满视口，退出后恢复提示符高度。
- [ ] 在提示符空闲（没有任何输出）时按 `Ctrl+\` 开关侧边栏：
      状态栏的列数必须跟着 pane 宽度变化，不能停在旧值。

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
   target/debug/anvil --mode vte --no-restore
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

- [ ] `Ctrl+Home` / `Ctrl+End`、`PageUp` / `PageDown` 可正常浏览。
- [ ] 在提示符处输入一条长命令后按 `Home`：光标回到行首，视口不动；按 `End`
      回到行尾。选中任一块后按 `Home` / `End`：选择跳到最旧 / 最新块。
- [ ] `Ctrl+Shift+A` 能选中全部块，无明显卡死。
- [ ] `Ctrl+Shift+K` 清空后没有大段空白、残留滚动范围或不可见的新块。
- [ ] 清空后连续执行 10 条命令，每条都正常出现。

### BM-14 卡片稳定性与快捷键归属

先生成若干块，其中至少一个来自进出 alt-screen 的命令：

```bash
sh -c "printf 'pre-alt line\n'; less /etc/hostname"   # 进入后按 q 退出
```

- [ ] 退出后停在提示符不动：卡片高度、输出行和历史滚动位置都不再逐帧抖动，
      `top` 中 anvil 的 CPU 回落到接近 0。
- [ ] 该卡片右侧的块内滚动条出现或消失时，卡片里的文本不重排、不换行位置变化。
- [ ] 制造 3 个失败块，连按 `Ctrl+Shift+X`：依次落在第 1、2、3 个失败块并回绕，
      而不是每次都回到最旧的那个。
- [ ] 不选中任何块直接按 `Ctrl+Shift+B`：最新块出现书签星标。
- [ ] 未添加任何书签时按 `Ctrl+,`：按键传给终端里的程序（用 `cat -v` 观察），
      不被 anvil 吞掉；添加书签后 `Ctrl+,` / `Ctrl+.` 恢复跳转。
- [ ] `Ctrl+Shift+K` 后从命令面板执行 `Undo clear blocks`，在恢复出来的卡片上
      右键：菜单正常弹出，复制 / 书签 / 回填都可用。重启恢复会话后同样可用。
- [ ] 悬停卡片：复制命令、复制输出、回填提示符三个按钮图标互不相同。
- [ ] 执行 `sleep 90`，卡片时长徽章显示 `1m30s`（不是 `2m`），悬停显示毫秒值。
- [ ] 向上翻两页离开提示符（跳转按钮出现），直接开始输入：视口自动回到提示符，
      跳转按钮和未读徽标一起消失，输入内容完整落在命令行上。
- [ ] 在 alt-screen 程序（如 `vim`）里输入不会触发上述回跳。
- [ ] 打开设置切换 **Compact Block Layout**：当前面板里已有的卡片和底部输入格
      立即变紧凑，再切回来恢复原状；状态栏的网格尺寸随之更新一次。
- [ ] 停在提示符执行 `ls`：右上角不闪出运行提示。执行 `sleep 20`：约 2 秒后
      右上角出现带秒表的提示条，悬停显示完整命令；点其中的停止按钮命令被中断
      （`^C`、exit 130），提示条随即消失。
- [ ] 命令运行中向上滚动：右上角提示条消失、顶部 sticky 条出现，两者不同时在场；
      滚回底部后又换回提示条。进入 alt-screen 程序期间两者都不出现。
- [ ] 卡片底部与边框之间有留白,输出最后一行不再贴住边框；命令行与输出的左边缘
      对齐（提示符 `❯` 现在在头部,不再把命令推右）；列对齐与实时面板一致。
- [ ] 制造一个 degraded 块(shell 未发 `D`)：头部出现 `inferred` 小标签,
      悬停它显示完整说明；正常块没有该标签,background block 也没有。
- [ ] `Alt+Shift+O`（或右键 `Fold Output`）折叠选中/最新块的输出,再按一次展开；
      折叠后显示 `▸ N lines hidden — click to show`。
- [ ] 右键 `Copy Directory` 复制该块的完整 cwd；`Go to Directory` 在提示符插入
      正确引号的 `cd …` 但不执行(路径含空格或单引号时也正确)；命令运行中该项置灰。
- [ ] 卡片 cwd 小标签悬停显示完整路径；`Copy Block as Markdown` 与
      `Export session as Markdown file` 都含 `**Directory:**` 行。
- [ ] `Ctrl+Shift+G` 搜索一个常见词：每行右侧显示 `exit:N · 时长 · …/目录`,
      失败行为红色；点 `Failed` 只剩失败块的命中,点 `Slow` 只剩慢块；
      按住 `Down` 走过 30 行,选中行始终可见,`Enter` 跳到你正在看的那一条。
- [ ] 在某个块的输出里拖选一段文字，然后在该块上右键：菜单第一项是
      `Copy Selection`,复制的正是那段文字；菜单弹出后原来的文本高亮消失,
      只剩卡片选中描边,此时按 `Ctrl+Shift+C` 复制的是整块(与屏幕一致)。
- [ ] `Ctrl+Shift+A` 全选后右键：最后一项显示 `Delete N Blocks`,点击后 N 个块
      全部消失,幸存块的书签、未读徽标和滚动位置都正常。
- [ ] 鼠标沿卡片列表上下移动：各卡片右侧的时间戳 / 耗时 / 退出码始终停在同一
      横坐标,不再左右跳动；点击这些按钮所在的空白处仍然是选中该块。
- [ ] 对某个块开启输出过滤后点"复制输出":剪贴板只含可见行,按钮提示
      `Filtered output copied`;此时 `Ctrl+Shift+F` 搜索一个被过滤掉的词,
      其它块的匹配计数不受影响、不会整体归零。
- [ ] 用一个没有加载 shell integration 的 shell 启动
      （`ANVIL_SHELL=/bin/bash anvil --mode block --no-restore`）：几秒后底部
      出现提示卡,写明该 shell 的 rc 文件和一行加载命令,可复制、可关闭；
      在终端里手动执行那行命令后回车,提示卡自动消失并开始正常分块。
      用 `anvil -e "ls"` 或 jsh 启动时不应出现该卡片。
- [ ] 在提示符或卡片之间的空白处右键：出现 `Copy` / `Paste` / `Select All Blocks`
      菜单；无选中内容时 `Copy` 置灰；点 `Paste` 把剪贴板内容插到提示符，
      菜单关闭后焦点回到输入格。
- [ ] 在某个卡片上右键：出现的仍是卡片自己的菜单（含 `Delete Block`），
      不是画布菜单。
- [ ] 运行 `less README.md` 期间右键：不弹任何菜单。
- [ ] 制造一个 degraded 块（shell 未发 OSC 133 `D`，卡片提示"推断"），
      `Ctrl+Shift+K` 清空后再执行一条普通命令：新卡片悬停**不应**再出现
      那条"状态由边界推断"的说明。

## 4. 问题记录模板

```text
用例编号：BM-__
结果：通过 / 失败
桌面环境：X11 / Wayland，桌面或窗口管理器版本
Shell：bash / zsh / fish / pwsh 及版本
anvil commit：git rev-parse --short HEAD
是否加载 shell integration：是 / 否
复现步骤：
实际结果：
期望结果：
日志或截图：
```

发现问题时，建议同时附上：

```bash
target/debug/anvil --doctor
RUST_LOG=anvil=debug target/debug/anvil --mode block --no-restore
```
