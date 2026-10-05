# Codex Switcher CLI

一个独立的 Rust 命令行账号管理工具，提供 `add`、`remove`、`edit`、`switch`，用于查看账号列表与实时用量的 `list`、`status`。

已移除 Tauri/React GUI、Web UI、托盘、后台进程监控、自动预热和自动更新。无需 Node.js、pnpm 或桌面环境，不运行后台服务。查看和关闭进程由 `ps`、`stop` 命令显式执行。

## 安装

需要 Rust 1.89 或更新版本：

```sh
cargo install --path . --locked
codex-switcher --help
```

也可以在项目内运行：

```sh
cargo run -- add personal
cargo run -- list
cargo build --release --locked
# 可执行文件：target/release/codex-switcher（Windows 为 .exe）
```

## 使用

```sh
# 保存当前 Codex auth.json 中的登录信息，不切换账号
codex-switcher add personal

# 从指定文件新增账号；省略名称时从邮箱或账号 ID 生成
codex-switcher add work --file /path/to/auth.json

# 使用保留的 ChatGPT OAuth/PKCE 流程登录，完成后保存账号
codex-switcher add work --login
# 不自动打开浏览器，手动访问终端输出的地址
codex-switcher add work --login --no-browser

# 从标准输入读取 API key，避免作为命令参数传递
codex-switcher add api --api-key-stdin < /path/to/api-key.txt

# 查看账号，* 表示当前 auth.json 对应的账号
codex-switcher list
codex-switcher list --json
codex-switcher ls                  # list 的别名

# 查看运行中的 Codex CLI、Codex 桌面端、app-server-daemon 和 ChatGPT
codex-switcher ps

# 先列出进程并确认后优雅关闭；-y/--yes 跳过确认
# 托管 app-server 通过 `codex app-server daemon stop` 关闭（直接终止会导致客户端无法重连）；
# 托管 daemon 的自动更新进程（app-server updater）只列出、不关闭
codex-switcher stop
codex-switcher stop --yes

# 按精确名称或完整 ID 切换
codex-switcher switch work
codex-switcher switch So Zhang       # 名称含空格时无需加引号
codex-switcher switch work --no-restart   # 只切换，不重启 app-server

# 修改名称、替换凭据，或同时修改
codex-switcher edit work --name company
codex-switcher edit company --file /path/to/new-auth.json
codex-switcher edit api --api-key-stdin < /path/to/new-api-key.txt

# 从保存的账号列表中删除，不登出 Codex，也不自动切换其他账号
codex-switcher remove personal
```

`add` 不自动切换；需要激活新账号时显式运行 `switch`。`edit` 保留账号 ID、创建时间和使用记录；替换当前账号的凭据时也会更新当前 `auth.json`。

运行中的 app-server 不会重新加载属于其他账号的 `auth.json`，因此 手动 `switch` 成功后会重启 app-server：确保 `[features] daemon_auto_start = true`，关闭客户端自行拉起的 app-server，再运行 `codex app-server daemon restart`，已连接的 CLI 会自动重连，进行中的任务会被中断。`edit` 不会触发重启。全局参数 `--no-restart` 跳过手动切换后的重启，`--codex-bin` 指定 Codex 可执行文件（默认 `codex`，PATH 中找不到时使用 `$CODEX_HOME/packages/app-server-daemon/current/bin/codex`）。重名、空名称、未知账号、无效认证文件和参数冲突会返回非零退出码。

OAuth 登录仅在命令执行期间启动本地回调监听器，默认端口为 `1455`，占用时选择空闲端口；等待最多五分钟，可用 Ctrl+C 取消。远程主机上的登录需要浏览器能够访问该主机的回调端口。

## 查看用量与重置时间

```sh
codex-switcher status                 # 查询所有已保存账号
codex-switcher status <ID>            # 只查询指定账号，也支持精确名称
codex-switcher status --json          # 全部账号，JSON 数组
codex-switcher status <ID> --json      # 单个账号，仍返回 JSON 数组
codex-switcher list                   # 本地账号列表，用于查看 ID
```

终端输出按账号分组，带缩进和用量进度条：左侧红色表示已用比例，右侧绿色表示剩余比例；当前账号和重置倒计时使用青色。颜色自动适配终端，管道或重定向输出禁用颜色，支持 `NO_COLOR`。也可手动选择：

```sh
codex-switcher status --color always
codex-switcher list --color never
```

`--json` 始终输出纯 JSON，不受颜色选项影响。

接口路径参照 [openai/codex 的 backend client](https://github.com/openai/codex/blob/c6c09fe29fb0b926ced5146e6ed47b6ff756c1a4/codex-rs/backend-client/src/client/rate_limit_resets.rs#L124)：

- 默认 base URL 为 `https://chatgpt.com/backend-api`，请求 `/wham/usage`。
- `https://chatgpt.com` 和 `https://chat.openai.com` 自动补上 `/backend-api`。
- 其他 base URL 不含 `/backend-api` 时，请求 `/api/codex/usage`。

可用 `status --base-url <URL>` 显式指定后端（提供 base URL，不是完整 usage 路径）。该后端会接收所选账号的 access token 和 ChatGPT account ID。请求使用 `Authorization: Bearer <access_token>`、`chatgpt-account-id` 和官方客户端的默认 `User-Agent: codex-cli`，不再模拟浏览器请求头。这里只对齐 HTTP 查询方式，不启动官方 TUI 的定时刷新逻辑。

`status` 按次请求 ChatGPT 的 usage 接口，显示每个账号的套餐、用量窗口（通常是 5 小时与每周）、已用和剩余百分比、重置时间、重置倒计时，JSON 中另保留接口返回的 credits，终端不显示 Credits。终端显示本地时区，JSON 的 `resets_at` 使用 UTC，`resets_in_seconds` 表示查询时距重置的秒数。

无参数时查询所有保存的账号；有 ID 或名称时只查询该账号。服务端未返回的窗口显示为不可用，不当作零用量。API key 账号标记为 `unsupported`，不请求 ChatGPT 用量接口。

请求超时或接口异常按账号标记为 `error`，继续查询其他账号；有查询失败时退出码为 1，`--json` 的标准输出仍为完整的结果数组。没有账号时返回空数组。找不到指定账号时直接报错。

查询优先使用当前本地凭据；遇到 HTTP 401 时重新读取凭据，必要时刷新并保存 token 后重试一次。HTTP 403 不触发刷新。查询不会切换当前账号、启动后台监控或发送预热请求。仅当刷新的是当前账号时同步其 `auth.json`。

`list`（也可用 `ls`）保留本地凭据状态与本地套餐信息，不联网、不写入文件。`status` 和 `list` 均不输出密钥或 token。

## 数据与切换

- 默认沿用原应用的 `~/.codex-switcher/accounts.json`，已有账号无需迁移。旧界面配置与监控缓存不再使用，也不会删除用户目录中的文件。
- Codex 凭据写入 `$CODEX_HOME/auth.json`，未设置时使用 `~/.codex/auth.json`。
- 切换前核对当前身份，并保存 Codex 已轮换的 OAuth token；切换目标 token 即将过期或已过期时请求刷新；显式用量查询遇到 HTTP 401 时也会按需刷新。保存轮换后的 refresh token 后才报告无效 ID token 错误。
- 账号命令使用文件锁避免同一账号库的并发写入；凭据文件采用原子替换，Unix 文件权限为 `0600`。
- `list` 不发起网络请求、不输出 token 或 API key。账号文件本身仍包含明文凭据。
- 切换通过写入文件生效；先退出正在使用该登录的 Codex 会话，切换后重新启动。`switch` 不会自动检测或关闭进程；可先用 `ps` 检查，必要时运行 `stop`。`stop` 只针对列出的进程请求优雅关闭，不强制结束仍在运行的进程。

可以显式指定隔离目录，参数可放在子命令前后：

```sh
codex-switcher --store-dir /path/to/profiles --codex-home /path/to/codex add personal
codex-switcher --store-dir /path/to/profiles --codex-home /path/to/codex switch personal
```

## 开发

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

测试使用临时目录、虚构凭据和本地模拟 HTTP 服务，不修改实际账号或请求真实 OAuth token。
