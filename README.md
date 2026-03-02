# UESTC Power Monitor

电子科技大学（UESTC）宿舍电费监控工具。

本项目旨在自动监控宿舍电费余额，将数据记录到 SQLite 数据库中进行持久化保存，并提供低余额报警功能，避免突然停电的尴尬。

## 功能特性

- 🔌 **自动轮询**: 定时获取电费余额和剩余电量。
- 💾 **数据持久化**: 自动将历史数据保存到 SQLite 数据库，方便后续分析。
- 🚨 **低余额报警**: 当余额低于设定阈值时，自动发送通知。
- 💓 **每日心跳**: 每天定时发送余额报告，确保监控正常运行。
- 📢 **多渠道通知**: 支持 Console、Webhook、Telegram Bot、Pushover、ntfy 和 Email (SMTP)，可同时启用多个通知渠道。
- 🕒 **统一时区**: 应用内时间默认使用 `Asia/Shanghai`，日志/通知/心跳/入库时间语义一致。
- 🐳 **Docker 支持**: 提供完整的 Docker 镜像构建和 Docker Compose 配置，支持 Docker Secrets。

## 快速开始

### 1. 环境准备

- [Rust](https://www.rust-lang.org/tools/install) (编译环境)

### 2. 获取代码

```bash
git clone https://github.com/yourusername/uestc-power-monitor.git
cd uestc-power-monitor
```

### 3. 配置文件

复制示例配置文件并进行修改：

```bash
cp config.toml.example config.toml
```

编辑 `config.toml`，填入你的学号、密码。数据库文件会在首次运行时自动创建。

### 4. 编译运行

```bash
# 开发模式运行
cargo run

# 生产模式构建并运行
cargo build --release
./target/release/uestc-power-monitor
```

### 5. Docker 部署 (推荐)

本项目支持 Docker 部署，包含自动构建和数据库配置。

1. **准备配置**: 复制 `config.toml.example` 为 `config.toml` 并填入账号信息。
2. **启动服务**:
   ```bash
   docker-compose up -d --build
   ```

## 配置详解

配置加载优先级：**环境变量 > Docker Secrets > 配置文件**。

时区规则：
- 默认使用 `Asia/Shanghai`
- 优先级：`UPM_TIMEZONE`（环境变量）> `timezone`（配置文件）> 默认值
- 若 `UPM_TIMEZONE` 或 `timezone` 非法，程序会记录告警并回退到 `Asia/Shanghai`
- 业务时间（日志、通知、心跳、`created_at`）不依赖系统 `TZ`

### 1. 配置文件 (config.toml)

完整配置项请参考 `config.toml.example`。

### 2. 环境变量

所有配置项均可通过环境变量设置，前缀为 `UPM_`。层级结构使用双下划线 `__` 分隔。

| 环境变量 | 对应配置项 | 说明 |
| --- | --- | --- |
| `UPM_USERNAME` | `username` | 学号 |
| `UPM_PASSWORD` | `password` | 密码 |
| `UPM_DATABASE_URL` | `database_url` | 数据库连接字符串 |
| `UPM_TIMEZONE` | `timezone` | 应用时区（IANA 名称，如 `Asia/Shanghai`） |
| `UPM_INTERVAL_SECONDS` | `interval_seconds` | 轮询间隔(秒) |
| `UPM_LOGIN_TYPE` | `login_type` | 登录方式 (password/wechat) |
| `UPM_COOKIE_FILE` | `cookie_file` | Cookie 文件路径 |
| `UPM_NOTIFY__ENABLED` | `notify.enabled` | 是否启用通知 (true/false) |
| `UPM_NOTIFY__THRESHOLD` | `notify.threshold` | 余额报警阈值 (元) |
| `UPM_NOTIFY__COOLDOWN_MINUTES` | `notify.cooldown_minutes` | 报警冷却时间 (分钟) |
| `UPM_NOTIFY__HEARTBEAT_ENABLED` | `notify.heartbeat_enabled` | 是否启用每日心跳 (true/false) |
| `UPM_NOTIFY__HEARTBEAT_HOUR` | `notify.heartbeat_hour` | 每日心跳时间 (0-23) |
| `UPM_NOTIFY__LOGIN_FAILURE_ENABLED` | `notify.login_failure_enabled` | 是否启用登录失败通知 (true/false) |
| `UPM_NOTIFY__FETCH_FAILURE_ENABLED` | `notify.fetch_failure_enabled` | 是否启用获取失败通知 (true/false) |
| `UPM_NOTIFY__NOTIFY_TYPE` | `notify.notify_type` | 单通道通知类型 (console/webhook/telegram/pushover/ntfy/email) |
| `UPM_NOTIFY__NOTIFY_TYPES` | `notify.notify_types` | 多通道通知类型 (逗号分隔，如 "telegram,ntfy,email") |
| `UPM_NOTIFY__WEBHOOK_URL` | `notify.webhook_url` | Webhook URL |
| `UPM_NOTIFY__TELEGRAM_BOT_TOKEN` | `notify.telegram_bot_token` | Telegram Bot Token |
| `UPM_NOTIFY__TELEGRAM_CHAT_ID` | `notify.telegram_chat_id` | Telegram Chat ID |
| `UPM_NOTIFY__PUSHOVER_API_TOKEN` | `notify.pushover_api_token` | Pushover App Token |
| `UPM_NOTIFY__PUSHOVER_USER_KEY` | `notify.pushover_user_key` | Pushover User Key |
| `UPM_NOTIFY__PUSHOVER_PRIORITY` | `notify.pushover_priority` | Pushover 默认优先级 (-2 到 2，默认 0；低余额告警固定为 2) |
| `UPM_NOTIFY__PUSHOVER_RETRY` | `notify.pushover_retry` | Pushover priority=2 时重试间隔秒数（最小 30） |
| `UPM_NOTIFY__PUSHOVER_EXPIRE` | `notify.pushover_expire` | Pushover priority=2 时总重试时长秒数（30-10800） |
| `UPM_NOTIFY__PUSHOVER_URL` | `notify.pushover_url` | Pushover 点击跳转 URL (可选) |
| `UPM_NOTIFY__NTFY_TOPIC_URL` | `notify.ntfy_topic_url` | ntfy Topic URL (完整发布地址，必须 https，且主机不能是/不能解析到 localhost 或内网 IP) |
| `UPM_NOTIFY__NTFY_TOKEN` | `notify.ntfy_token` | ntfy 访问令牌（可选，发送时使用 Bearer Token） |
| `UPM_NOTIFY__NTFY_PRIORITY` | `notify.ntfy_priority` | ntfy 默认优先级 (1 到 5，默认 3；低余额告警固定为 5) |
| `UPM_NOTIFY__NTFY_TAGS` | `notify.ntfy_tags` | ntfy 标签 (逗号分隔，如 "warning,skull") |
| `UPM_NOTIFY__NTFY_CLICK_ACTION` | `notify.ntfy_click_action` | ntfy 点击跳转 URL (可选) |
| `UPM_NOTIFY__NTFY_ICON` | `notify.ntfy_icon` | ntfy 图标 URL (可选) |
| `UPM_NOTIFY__NTFY_USE_MARKDOWN` | `notify.ntfy_use_markdown` | ntfy 是否启用 Markdown (true/false) |
| `UPM_NOTIFY__SMTP_SERVER` | `notify.smtp_server` | SMTP 服务器地址 |
| `UPM_NOTIFY__SMTP_PORT` | `notify.smtp_port` | SMTP 端口 |
| `UPM_NOTIFY__SMTP_USERNAME` | `notify.smtp_username` | SMTP 用户名 |
| `UPM_NOTIFY__SMTP_PASSWORD` | `notify.smtp_password` | SMTP 密码 |
| `UPM_NOTIFY__SMTP_FROM` | `notify.smtp_from` | 发件人地址 |
| `UPM_NOTIFY__SMTP_TO` | `notify.smtp_to` | 收件人地址 (逗号分隔) |
| `UPM_NOTIFY__SMTP_ENCRYPTION` | `notify.smtp_encryption` | SMTP 加密方式 (starttls/tls/none) |

> `ntfy_actions` 为复杂对象数组，建议在 `config.toml` 中配置（示例见 `config.toml.example`）。

### 3. Docker Secrets

支持从 `/run/secrets/` 目录读取敏感信息，适合 Docker Swarm 或 Kubernetes 环境。

- `username`: `/run/secrets/username`
- `password`: `/run/secrets/password`
- `service_url`: `/run/secrets/service_url`
- `database_url`: `/run/secrets/database_url`

## 通知渠道配置

### 单通道通知（向后兼容）

使用 `notify_type` 配置单个通知渠道：

```toml
[notify]
enabled = true
notify_type = "telegram"  # 可选: console, webhook, telegram, pushover, ntfy, email
```

### 多通道通知（新功能）

使用 `notify_types` 同时启用多个通知渠道：

```toml
[notify]
enabled = true
notify_types = ["telegram", "ntfy", "pushover"]  # 同时发送到多个渠道
```

**通过环境变量配置多通道：**

```bash
UPM_NOTIFY__NOTIFY_TYPES="telegram,ntfy,pushover"
```

**注意事项：**
- 如果同时设置了 `notify_type` 和 `notify_types`，则 `notify_types` 优先
- 每个通知渠道独立运行，一个渠道失败不影响其他渠道
- 缺少必要配置的渠道会被自动跳过（如 Telegram 缺少 bot_token）
- 所有渠道都会收到相同的通知内容

### 通知渠道说明

1. **Console**: 输出到控制台日志，无需额外配置
2. **Webhook**: 发送 JSON 数据到指定 URL，需配置 `webhook_url`
3. **Telegram**: 通过 Telegram Bot 发送消息，需配置 `telegram_bot_token` 和 `telegram_chat_id`
4. **Pushover**: 调用 Pushover API 发送通知，需配置 `pushover_api_token` 与 `pushover_user_key`（低余额告警固定最高优先级 `2`；其他事件使用 `pushover_priority`；`priority=2` 时还需 `pushover_retry` / `pushover_expire`）
5. **ntfy**: 通过 ntfy Topic 推送通知，需配置 `ntfy_topic_url`（必须 https，且主机不能是/不能解析到 localhost 或内网 IP；低余额告警固定最高优先级 `5`；其他事件使用 `ntfy_priority`；可选 `ntfy_token`、tags / click / icon / actions / markdown）
6. **Email**: 通过 SMTP 发送邮件，需配置完整的 SMTP 参数（服务器、端口、认证信息等）

## 数据表结构

程序会自动创建 `power_records` 表，主要包含以下字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| id | INTEGER | 主键（自增） |
| remaining_energy | REAL | 剩余电量 (度) |
| remaining_money | REAL | 剩余金额 (元) |
| meter_room_id | TEXT | 电表房间ID |
| room_display_name | TEXT | 房间显示名称 |
| created_at | TEXT | 记录时间（RFC3339，含时区偏移，如 `+08:00`） |
| ... | ... | 其他位置信息字段 |

## License

MIT
