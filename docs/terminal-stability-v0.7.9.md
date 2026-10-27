# Probe Shell v0.7.9 — 终端稳定性验证

本版本只调整终端解析依赖及回归测试，不重构文件浏览或设备画像。

## 自动检查

- `cargo check --locked`：确认 Cargo.toml 与 Cargo.lock 完全一致。
- `cargo test --locked`：包含 `alt_screen_shrink_then_exit_keeps_cursor_in_bounds` 和 `cjk_selection_and_find_stay_aligned_after_normal_screen_reflow`。
- 对照 Windows 和 Linux 的 GitHub Actions 结果，禁止在失败时发布版本标签。

## 人工回归（建议 Windows 11）

| 测试 | 操作 | 预期 |
| --- | --- | --- |
| 备用屏幕退出 | SSH 连接 Linux/NAS，运行 `vim`、`nano`、`htop`；缩窄终端后退出 | 不崩溃、光标处于终端可视网格内、后续命令可正常输入 |
| 连续缩放 | 在 `vim`、`htop` 内多次左右拖动窗口，再退出 | 不白屏、不闪退、不残留旧画面 |
| 普通屏幕重排 | `printf '提示: 1pctl\\n'`，左右缩放，搜索和选择 `1pctl` | 中文宽字符后面的高亮/复制列不偏移 |
| OpenWrt / Dropbear | 连接仅提供 SSH 的路由器，浏览 `/etc`、`/tmp`，执行少量命令 | 原有 SSH 文件浏览回退可用；登录不注入 `PROMPT_COMMAND` |
| 多标签页 | 同时打开路由器/NAS 的终端；调整布局，切换/关闭标签 | 无状态错乱、无偶发崩溃 |
| macOS / Linux | 在可用环境重复全屏终端退出与窗口缩放 | 与 Windows 行为一致 |

## 问题发生时收集

- 崩溃发生的设备/系统版本、终端行列数、远端程序（vim/htop/tmux 等）、复现步骤。
- 程序目录 `log/crash.log` 和 `log/error.log`，提交时注意隐藏主机地址、用户名与令牌。
- 如果 CI 因上游 API 变化失败，先修复并重新验证，再合并或打 `v0.7.9` 标签。

## 发布原则

- 必须确认 `Cargo.toml` 中版本为 `0.7.9`，且对应 commit 已经过自动检查。
- 合并到 `main` 并验证后，才手动创建 `v0.7.9` 标签触发多平台 Release。
- 不把本版本当作已在真实路由器/Windows 环境验证的正式成品，除非人工回归完成。
