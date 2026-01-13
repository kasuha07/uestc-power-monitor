# UESTC Power Monitor

电子科技大学（UESTC）宿舍电费监控工具。

本项目旨在自动监控宿舍电费余额，将数据记录到 SQLite 数据库中进行持久化保存，并提供低余额报警功能，避免突然停电的尴尬。

## 功能特性

- 🔌 **自动轮询**: 定时获取电费余额和剩余电量。
- 💾 **数据持久化**: 自动将历史数据保存到 SQLite 数据库，方便后续分析。
- 🚨 **低余额报警**: 当余额低于设定阈值时，自动发送通知。
- 💓 **每日心跳**: 每天定时发送余额报告，确保监控正常运行。
- 📢 **多渠道通知**: 目前支持 Telegram Bot、Webhook 和控制台输出。
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

### 1. 配置文件 (config.toml)

完整配置项请参考 `config.toml.example`。

### 2. 环境变量

所有配置项均可通过环境变量设置，前缀为 `UPM_`。层级结构使用双下划线 `__` 分隔。

| 环境变量 | 对应配置项 | 说明 |
| --- | --- | --- |
| `UPM_USERNAME` | `username` | 学号 |
| `UPM_PASSWORD` | `password` | 密码 |
| `UPM_DATABASE_URL` | `database_url` | 数据库连接字符串 |
| `UPM_INTERVAL_SECONDS` | `interval_seconds` | 轮询间隔(秒) |
| `UPM_LOGIN_TYPE` | `login_type` | 登录方式 (password/wechat) |
| `UPM_COOKIE_FILE` | `cookie_file` | Cookie 文件路径 |
| `UPM_NOTIFY__ENABLED` | `notify.enabled` | 是否启用通知 (true/false) |
| `UPM_NOTIFY__THRESHOLD` | `notify.threshold` | 余额报警阈值 (元) |
| `UPM_NOTIFY__COOLDOWN_MINUTES` | `notify.cooldown_minutes` | 报警冷却时间 (分钟) |
| `UPM_NOTIFY__HEARTBEAT_ENABLED` | `notify.heartbeat_enabled` | 是否启用每日心跳 (true/false) |
| `UPM_NOTIFY__HEARTBEAT_HOUR` | `notify.heartbeat_hour` | 每日心跳时间 (0-23) |
| `UPM_NOTIFY__NOTIFY_TYPE` | `notify.notify_type` | 通知类型 (console/webhook/telegram) |
| `UPM_NOTIFY__WEBHOOK_URL` | `notify.webhook_url` | Webhook URL |
| `UPM_NOTIFY__TELEGRAM_BOT_TOKEN` | `notify.telegram_bot_token` | Telegram Bot Token |
| `UPM_NOTIFY__TELEGRAM_CHAT_ID` | `notify.telegram_chat_id` | Telegram Chat ID |

### 3. Docker Secrets

支持从 `/run/secrets/` 目录读取敏感信息，适合 Docker Swarm 或 Kubernetes 环境。

- `username`: `/run/secrets/username`
- `password`: `/run/secrets/password`
- `service_url`: `/run/secrets/service_url`
- `database_url`: `/run/secrets/database_url`

## 数据表结构

程序会自动创建 `power_records` 表，主要包含以下字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| id | INTEGER | 主键（自增） |
| remaining_energy | REAL | 剩余电量 (度) |
| remaining_money | REAL | 剩余金额 (元) |
| meter_room_id | TEXT | 电表房间ID |
| room_display_name | TEXT | 房间显示名称 |
| created_at | DATETIME | 记录时间 |
| ... | ... | 其他位置信息字段 |

## License

MIT
