# UESTC Power Monitor

电子科技大学（UESTC）宿舍电费监控工具（Rust）。

该项目会定时拉取宿舍电费/电量信息，写入 SQLite 做历史留存，并在余额过低或系统异常时通过多种渠道告警，避免断电。

---

## 功能特性

- **定时监控**：按固定间隔轮询电费数据（默认 600 秒）。
- **自动重试与会话恢复**：请求失败自动重试；检测到会话失效会自动重新登录。
  电费网关要求直连内网查询时，自动跟随服务端下发的重试地址（sign/data 由服务端
  生成，客户端无需也无法自行计算）。
- **Web 登录页**：启动后在浏览器输入账号密码、微信扫码或完成短信/企微二次认证。
  缺少凭证、登录失败、会话过期时容器保持运行；登录成功后自动恢复监控。
- **reauth 二次认证支持**：Web 与终端登录共用认证协议，支持选择验证方式和记住设备。
- **SQLite 持久化**：每次采样写入 `power_records`，便于后续统计分析。
- **多事件通知**：
  - 低余额告警
  - 启动通知
  - 每日心跳
  - 登录失败告警
  - 连续拉取失败告警
- **多渠道通知**：支持 Console / Webhook / Telegram / Pushover / ntfy / Email，可并行多通道发送。
- **统一时区语义**：默认 `Asia/Shanghai`，日志、通知、入库时间统一。
- **容器化部署**：支持 Docker、docker compose、Docker Secrets。

---

## 项目结构

```text
src/
├── main.rs      # 日志初始化与程序入口
├── lib.rs       # 主循环（抓取 -> 入库 -> 通知）
├── api.rs       # 登录、会话检查、数据抓取
├── db.rs        # SQLite 初始化与写入
├── notify.rs    # 通知管理与各通知通道实现
├── config.rs    # 配置加载（文件/Secrets/环境变量）
├── time.rs      # 应用时区与时间工具
└── utils.rs     # 重试工具
```

---

## 工作流程

1. 加载配置并初始化 SQLite、Web 登录页和通知模块。
2. 尝试恢复已有 Cookie；没有会话时等待浏览器登录，不退出、不反复尝试密码。
3. 用户在页面完成认证后，验证电费门户会话并加密保存 Cookie，立即唤醒监控。
4. 定时采集电费数据，写入数据库并判断通知条件。
5. 会话过期时暂停采集，保留最近读数并等待重新登录；网络/上游故障按采集周期重试。
6. 捕获 `SIGINT/SIGTERM` 后退出。

设置 `web.enabled = false` 可恢复原有终端启动、自动重登录行为。

---

## 快速开始（本地）

### 1）准备环境

- Rust（建议稳定版）
- 可访问 `online.uestc.edu.cn`

### 2）准备配置

```bash
cp config.toml.example config.toml
```

按需修改 `config.toml`：

- `username` / `password`（可留空，改用 Web 登录）
- `database_url`（如 `sqlite://power_monitor.db`）
- `notify` 下的通知配置

默认无需在配置文件里保存账号密码。启动后打开 `http://127.0.0.1:8080`，
输入启动日志显示的 **Web 访问密钥**，再登录学校账号。
密码只在内存中用于本次认证，页面不会把密码写入配置文件。

`login --force` 子命令仍支持终端输入账号密码；禁用 Web 后，默认启动也保留终端交互。

### 3）运行

```bash
cargo run
```

生产构建：

```bash
cargo build --release
./target/release/uestc-power-monitor
```

---

## Docker 部署

### 使用 compose（推荐）

准备配置文件和可写数据目录。镜像使用 UID/GID `65532` 的非 root 用户，绑定挂载的
`data/` 需要允许该用户写入；以下命令适用于新建目录，已有数据请按实际属主处理：

```bash
cp config.toml.example config.toml
mkdir -p data
sudo chown 65532:65532 data
# 非 root 用户需要读取配置文件，请保证文件可读。
docker compose up -d --build
docker compose logs app
```

如果已有 `data/` 属于宿主机用户，可在项目 `.env` 中设置实际 UID/GID，让容器使用
相同用户运行，无需修改现有数据属主。例如 `stat -c '%u:%g' data` 返回 `1000:1000` 时：

```dotenv
UPM_UID=1000
UPM_GID=1000
```

修改后执行 `docker compose up -d`。`.env` 不纳入 Git，未设置时仍使用 `65532:65532`。
属主不匹配会导致 `Permission denied` 并反复重启；数据目录需要可写，配置文件需要可读。

当前 compose 默认从本地源码构建，并挂载：

- `./config.toml -> /app/config.toml`（只读）
- `./data -> /app/data`（数据库、加密 Cookie、Cookie 密钥和 Web 访问密钥）

启动后访问 **http://127.0.0.1:8080**，输入日志中的 Web 访问密钥，再在页面：

1. 输入账号密码，或选择微信扫码。
2. 若需要二次认证，选择微信扫码、短信或企微验证码等可用方式。
3. 登录完成后监控立即开始，无需重启容器。

页面显示数据库中最近 10 次成功采集记录（时间、宿舍、电费、电量），重启后仍可查看。
“立即刷新”会使用当前会话立即采集一次；认证或采集期间不可重复触发。采集失败时保留
已保存的历史记录，新的成功读数会自动追加到历史记录中。

无凭证、密码错误、认证超时或 Cookie 过期时，容器保持运行并等待人工处理。
同一时间只允许一个认证操作，扫码期间可取消后重新发起。验证码重发间隔为 60 秒。

### 远程访问

compose 默认将端口映射到宿主机回环地址。服务器部署可使用 SSH 转发：

```bash
ssh -L 8080:127.0.0.1:8080 your-server
```

然后在本机浏览器访问 `http://127.0.0.1:8080`。如需直接远程开放，请在 HTTPS 反代后
访问，并保留访问密钥保护；不要在公网明文 HTTP 上传学校密码。

### 页面访问密钥

默认自动生成 64 字符随机密钥，保存在 `data/web-access-token`，权限 `0600`。
密钥在程序启动日志中显示；输入后仅保存在当前浏览器标签页的会话存储中。
也可以通过 `UPM_WEB__ACCESS_TOKEN` 或 `web.access_token` 指定至少 32 字符的固定密钥。
所有认证接口和状态查询都需要密钥，且禁止跨来源请求；响应禁止缓存。

`/healthz` 是进程存活检查，即使等待登录也返回 `200`，不会因为没有凭证触发重启。

### 保留终端登录方式

Web 默认启用，也可通过配置 `web.enabled = false` 或环境变量 `UPM_WEB__ENABLED=false`
关闭。独立登录命令仍可使用：

```bash
docker compose run --rm app /usr/local/bin/uestc-power-monitor login --force
```

Web 模式由同一个进程管理登录和监控，建议使用页面完成恢复。关闭 Web 时，运行期
reauth 的恢复仍使用独立 `login --force` 命令和 Cookie 重载机制。

---

## 配置说明

### 配置来源优先级

1. 环境变量（`UPM_` 前缀）
2. Docker Secrets（`/run/secrets/*`）
3. 配置文件（`config.toml`）
4. Web 页面输入（默认）或终端交互输入（`login` 子命令 / 禁用 Web 时）

> `UPM_TIMEZONE` 会在反序列化后再次覆盖，保证时区优先级生效。

### 时区规则

- 默认时区：`Asia/Shanghai`
- 要求使用 IANA 时区名（如 `Asia/Shanghai`、`UTC`）
- 若配置非法，程序会告警并回退到默认时区

### 关键配置项

| 配置项 | 说明 | 默认值 |
|---|---|---|
| `interval_seconds` | 轮询间隔（秒） | `600` |
| `timezone` | 应用时区 | `Asia/Shanghai` |
| `login_type` | 登录方式：`password` / `wechat` | `password` |
| `reauth_trust_device` | Web 的“记住此设备”勾选默认值，及终端提问的回车默认值 | `false` |
| `cookie_file` | 加密 Cookie 持久化文件 | `uestc_cookies.json` |
| `cookie_encryption_key` | 显式 Cookie 加密密钥；无凭证时自动生成并持久化，已有密码派生方式兼容 | 自动解析 |
| `web.enabled` | 启用 Web 登录页 | `true` |
| `web.bind` | Web 监听地址（Compose 覆盖为 `0.0.0.0:8080`） | `127.0.0.1:8080` |
| `web.access_token` | 页面访问密钥，至少 32 字符；未指定则随机生成并持久化 | 自动生成 |
| `notify.enabled` | 是否启用通知 | `false` |
| `notify.threshold` | 低余额阈值（元） | `5.0` |
| `notify.cooldown_minutes` | 低余额重复提醒冷却（分钟） | `520` |
| `notify.startup_enabled` | 启动通知开关 | `false` |
| `notify.login_retry_failure_enabled` | 登录重试失败通知开关（会话失效后重登连续失败时提醒） | `false` |
| `notify.login_retry_failure_threshold` | 登录重试失败连续轮次阈值 | `3` |
| `notify.login_retry_failure_cooldown_minutes` | 登录重试失败通知滚动冷却（分钟） | `1440` |
| `notify.reauth_pending_enabled` | reauth 待人工通知开关（运行期触发多因子且无人值守时） | `false` |
| `notify.reauth_pending_cooldown_minutes` | reauth 待人工重复提醒冷却（分钟） | `30` |
| `notify.reauth_resolved_enabled` | 人工完成 reauth 会话恢复后的确认通知开关 | `false` |
| `notify.heartbeat_enabled` | 每日心跳开关 | `false` |
| `notify.heartbeat_hours` | 每日心跳小时（0-23，支持单值或数组，兼容 `heartbeat_hour`） | `[9]` |
| `notify.retry_attempts` | 每个通知通道最大尝试次数 | `3` |
| `notify.retry_initial_delay_seconds` | 通知失败后的首次退避等待秒数 | `2` |
| `notify.retry_max_delay_seconds` | 通知指数退避最大等待秒数 | `60` |
| `notify.request_timeout_seconds` | 单次通知请求/SMTP 发送超时秒数 | `15` |

完整配置请直接参考：`config.toml.example`。

### 环境变量示例

```bash
UPM_USERNAME=2023xxxxxxx
UPM_PASSWORD=your_password
UPM_DATABASE_URL=sqlite://data/power_monitor.db
UPM_TIMEZONE=Asia/Shanghai
# 可选：指定独立 Cookie 加密密钥；无凭证时也可由程序自动生成
UPM_COOKIE_ENCRYPTION_KEY=change-me-to-a-long-random-secret
UPM_NOTIFY__ENABLED=true
UPM_NOTIFY__STARTUP_ENABLED=true
UPM_NOTIFY__NOTIFY_TYPES=telegram,ntfy,email
```

`notify` 子项使用 `__` 分隔层级（例如 `UPM_NOTIFY__THRESHOLD`）。

### Docker Secrets 支持

可选 secrets（存在即读取）：

- `/run/secrets/username`
- `/run/secrets/password`
- `/run/secrets/cookie_encryption_key`
- `/run/secrets/service_url`（当前代码中预留）
- `/run/secrets/database_url`

### Cookie 持久化安全

- Cookie 文件使用 AES-256-GCM 加密后保存，文件权限在 Unix 平台上会设置为 `0600`。
- 兼容旧版明文 Cookie 文件：启动时若检测到旧格式（未加密的 JSON 数组），会自动迁移为加密格式并继续使用，无需重新登录。
- 损坏的 Cookie 文件会被忽略，并在下次成功登录后写成新的加密格式。
- `password` 登录如果没有显式配置 `cookie_encryption_key`，会使用账号和密码作为密钥材料派生加密密钥。
- 未配置账号密码、且没有显式密钥时，自动生成 `<cookie_file>.key`，权限 `0600`。
  后续启动与 CLI 登录优先复用该密钥；请与 Cookie 文件一起备份，丢失后需要重新登录。
- 已配置账号密码、尚无自动密钥文件时保留原有密码派生方式，以兼容现有 Cookie。
  已有 Cookie 使用显式密钥时，请继续配置同一密钥。
- 读取失败不会删除已有加密 Cookie 文件；成功登录会写入新的加密会话。

### 网络与登录超时

所有 HTTP 请求都有超时保护，避免半开连接让监控循环永久阻塞：

| 项 | 值 |
| --- | --- |
| 建连超时 | 10 秒 |
| 单次请求总超时 | 30 秒 |
| 扫码轮询单次请求超时 | 30 秒 |
| 等待扫码总时长上限 | 5 分钟 |
| 轮询连续失败容忍次数 | 5 次（偶发抖动不会作废二维码） |
| 连续未知状态码容忍次数 | 10 次（接口变更时不会无限轮询） |

Web 模式一次提交只尝试一次认证，失败后显示状态并等待人工，避免反复提交错误密码。
禁用 Web 的终端启动模式保留原有三次重试和失败退出行为。

### 微信扫码与二次认证

默认 Web 模式下，微信登录与微信二次认证的二维码直接显示在页面，认证库在后台
轮询扫码状态，五分钟内未完成会失败。页面轮询状态不会阻塞扫码流程。
密码登录遇到 reauth 时，页面列出账号已开通且程序支持的方式：微信扫码、短信/企微等
动态码、密码二次认证。可以勾选“记住此设备”，默认值来自 `reauth_trust_device`。

会话过期后，页面显示等待登录，暂停采集并保留最近读数。通知使用现有
`ReauthPending` 冷却设置；再次认证并验证业务会话后自动恢复，发送 `ReauthResolved`。
启动第一次登录后的通知使用 `Startup`。

禁用 Web 时，原有终端模式仍支持 `login --force`、`login --type wechat`、`logout`：
运行期触发 reauth 后等待人工，独立登录保存 Cookie 后按采集周期恢复。
默认 Web 模式请通过页面重新认证，避免多个进程同时更新 Cookie。

---

## 通知系统

### 事件类型

- **LowBalance**：余额低于阈值时触发（支持冷却与边沿触发逻辑）
- **Startup**：服务启动后首次成功拉取时触发
- **Heartbeat**：每天在一个或多个指定小时发送状态心跳
- **LoginFailure**：认证失败时发送（Web 模式保持运行）
- **LoginRetryFailure**：运行期会话失效后重登连续失败达到阈值时发送（滚动冷却，默认一天最多一次）
- **ReauthPending**：运行期触发 reauth（多因子）且无人值守无法交互时发送（首次立即、之后按冷却重复，默认 30 分钟）
- **ReauthResolved**：人工完成 reauth、daemon 会话恢复后发送（一次性确认）
- **ConsecutiveFetchFailures**：连续抓取失败达到阈值后发送

### 通知通道

- `console`
- `webhook`
- `telegram`
- `pushover`
- `ntfy`
- `email`

可通过：

- `notify_type`（单通道，向后兼容）
- `notify_types`（多通道，优先级更高）

可靠性行为：

- 每个通道独立重试，使用指数退避并限制最大退避时间
- 单次通知发送有超时保护，避免某个通道长期阻塞
- 仅当至少一个通道发送成功时才会消耗启动/心跳/低余额/连续失败/登录重试失败通知状态；全部失败时会在后续轮询继续尝试

### 安全限制（Webhook / ntfy）

为避免 SSRF 风险，URL 校验包含：

- 必须为 `https`
- 禁止 `localhost`、`.local`、内网/回环/链路本地地址
- 域名解析后地址仍需为公网地址
- HTTP 客户端禁用重定向并执行 DNS 绑定解析

### Email 限制

- 仅支持 `starttls` 或 `tls`
- `smtp_encryption = "none"` 会被拒绝（不安全）

---

## 数据库结构

启动时自动创建 `power_records`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | INTEGER | 主键自增 |
| `remaining_energy` | REAL | 剩余电量（kWh） |
| `remaining_money` | REAL | 剩余金额（CNY） |
| `meter_room_id` | TEXT | 控电房间编号 |
| `room_display_name` | TEXT | 房间显示名 |
| `room_id` | TEXT | 房间 ID |
| `building_id` | TEXT | 楼栋 ID |
| `campus_id` | TEXT | 校区 ID |
| `room_number` | TEXT | 房间号 |
| `created_at` | TEXT | RFC3339 时间戳（含时区偏移） |

---

## 开发与测试

```bash
cargo fmt
cargo clippy --all-targets --all-features
cargo test
```

当前测试覆盖：

- 配置加载与时区优先级
- 时间格式与时区偏移
- Webhook/ntfy 的安全 URL 校验
- SMTP 加密模式限制
- 入库时间格式正确性
- Web 无凭证等待、访问密钥与来源校验、并发认证和取消
- 本地 HTTPS 模拟学校/微信：密码登录、二次认证、验证码重发冷却、扫码与 Cookie 恢复
- Web 登录唤醒监控、会话过期暂停与再次登录恢复

---

## 常见问题

### 1）页面显示等待登录或登录失败

- 打开 Web 登录页并输入启动日志中的访问密钥。
- 检查账号密码；若学校要求密码登录图形验证码，可改用微信扫码。
- 若需要二次认证，按页面提示选择方式并完成验证。
- 检查部署环境能否访问学校认证平台和电费门户。
- 检查 `data/` 对容器非 root 用户是否可写，以及端口是否被其他程序占用。

### 2）收到 ReauthPending 通知

默认 Web 模式下，打开页面重新登录即可，认证完成后自动恢复监控。
关闭 Web 的终端模式请运行 `uestc-power-monitor login --force`。

### 3）没有收到通知

- 确认 `notify.enabled = true`
- 确认通道参数完整（如 Telegram token/chat_id）
- 低余额通知受阈值与冷却时间影响

### 4）Webhook/ntfy URL 被拒绝

- 需使用公网 `https` 地址
- 不可指向 localhost/内网地址或解析到内网 IP

### 5）更换登录方式后无法读取 Cookie

旧明文 Cookie 会迁移为加密格式。若更换了 `cookie_encryption_key` 或丢失自动密钥，
请恢复原密钥或重新登录生成新会话。

### 6）日志出现 "electricity service reported a business failure"

这是电费网关的风控/防重放机制：响应体的 `d` 是 `"失败{...}"` 字符串（内含房间号、
时间戳与内网直连地址），而不是数据对象。这不是会话问题（信封 `e=0`），重新登录
没有用。程序会**自动跟随载荷里服务端下发的直连查询地址**（sign/data 已由服务端
生成好）再取一次读数；仍失败时计入连续抓取失败，恢复依赖下一轮轮询。直连地址的
query 是签名材料，不会写入日志。

---

## License

MIT
