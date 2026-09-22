# Codex Switcher CLI

一个独立的 Rust 命令行账号管理工具，提供 `add`、`remove`、`edit`、`switch`，以及用于查看账号列表与本地登录状态的 `list`、`status`。

已移除 Tauri/React GUI、Web UI、托盘、用量与进程监控、自动预热和自动更新。无需 Node.js、pnpm 或桌面环境，不运行后台服务。

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

# 按精确名称或完整 ID 切换
codex-switcher switch work

# 修改名称、替换凭据，或同时修改
codex-switcher edit work --name company
codex-switcher edit company --file /path/to/new-auth.json
codex-switcher edit api --api-key-stdin < /path/to/new-api-key.txt

# 从保存的账号列表中删除，不登出 Codex，也不自动切换其他账号
codex-switcher remove personal
```

`add` 不自动切换；需要激活新账号时显式运行 `switch`。`edit` 保留账号 ID、创建时间和使用记录；替换当前账号的凭据时也会更新当前 `auth.json`。重名、空名称、未知账号、无效认证文件和参数冲突会返回非零退出码。

OAuth 登录仅在命令执行期间启动本地回调监听器，默认端口为 `1455`，占用时选择空闲端口；等待最多五分钟，可用 Ctrl+C 取消。远程主机上的登录需要浏览器能够访问该主机的回调端口。

## 查看状态

```sh
codex-switcher status                 # 当前登录及本地凭据状态
codex-switcher status company         # 指定名称或完整 ID 的账号详情
codex-switcher status --json          # 供脚本读取的 JSON
codex-switcher status company --json
codex-switcher list                   # 列表中显示认证方式、凭据状态与本地套餐信息
```

`status` 显示账号、邮箱、认证方式、本地套餐信息、ID/access token 的到期时间、是否保存 refresh token、添加与最近切换时间、账号文件与认证文件路径。`login_status` 描述当前本地登录：`managed` 表示匹配已保存账号，`unmanaged` 表示凭据未匹配账号库，`not_logged_in` 表示没有本地登录；指定账号时，`account` 是该账号详情，`is_active` 表示它是否为当前账号。

凭据状态包括 `api_key_present`（API key 已配置）、`not_expired`（可解析的 token 尚未过期）、`expiring_soon`（60 秒内到期）、`expired`、`unknown`（无法判断到期时间）及 `missing`（缺少凭据）。这些状态仅根据本地文件计算，不验证服务端会话、API key 或 JWT 签名；无法解析的 token 到期时间显示为未知。本地套餐信息可能已过时，不表示实时订阅或额度。

`status` 和 `list` 都只读本地文件，不刷新 token、不查询用量、不启动监控，也不输出密钥或 token。损坏或无法读取的文件会报错并返回非零退出码。

## 数据与切换

- 默认沿用原应用的 `~/.codex-switcher/accounts.json`，已有账号无需迁移。旧界面配置与监控缓存不再使用，也不会删除用户目录中的文件。
- Codex 凭据写入 `$CODEX_HOME/auth.json`，未设置时使用 `~/.codex/auth.json`。
- 切换前核对当前身份，并保存 Codex 已轮换的 OAuth token；仅在切换目标 token 即将过期或已过期时请求刷新。保存轮换后的 refresh token 后才报告无效 ID token 错误。
- 账号命令使用文件锁避免同一账号库的并发写入；凭据文件采用原子替换，Unix 文件权限为 `0600`。
- `list` 不发起网络请求、不输出 token 或 API key。账号文件本身仍包含明文凭据。
- 切换通过写入文件生效；先退出正在使用该登录的 Codex 会话，切换后重新启动。CLI 不检测、终止或重启任何进程。

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

测试使用临时目录和虚构凭据，不修改实际账号或请求真实 OAuth token。
