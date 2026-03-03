# UESTC Power Monitor

电子科技大学（UESTC）宿舍电费监控工具（Rust）。

该项目会定时拉取宿舍电费/电量信息，写入 SQLite 做历史留存，并在余额过低或系统异常时通过多种渠道告警，避免断电。

---

## 功能特性

- **定时监控**：按固定间隔轮询电费数据（默认 600 秒）。
- **自动重试与会话恢复**：请求失败自动重试；检测到会话失效会自动重新登录。
- **SQLite 持久化**：每次采样写入 `power_records`，便于后续统计分析。
- **多事件通知**：
  - 低余额告警
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

1. 启动并加载配置（优先级：**环境变量 > Docker Secrets > 配置文件**）。
2. 初始化 API 服务并登录 UESTC 平台。
3. 初始化 SQLite 连接池并创建表。
4. 进入循环：
   - 拉取电费数据
   - 写入数据库
   - 判断并发送通知
   - 休眠到下一个轮询周期
5. 捕获 `SIGINT/SIGTERM` 后优雅退出。

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

- `username` / `password`
- `database_url`（如 `sqlite://power_monitor.db`）
- `notify` 下的通知配置

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

```bash
docker compose up -d --build
```

默认 compose 文件使用镜像：

- `ghcr.io/kasuha07/uestc-power-monitor:latest`

并挂载：

- `./config.toml -> /app/config.toml`
- `./data -> /app/data`

---

## 配置说明

### 配置来源优先级

1. 环境变量（`UPM_` 前缀）
2. Docker Secrets（`/run/secrets/*`）
3. 配置文件（`config.toml`）

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
| `cookie_file` | Cookie 持久化文件 | `uestc_cookies.json` |
| `notify.enabled` | 是否启用通知 | `false` |
| `notify.threshold` | 低余额阈值（元） | `5.0` |
| `notify.cooldown_minutes` | 低余额重复提醒冷却（分钟） | `520` |
| `notify.heartbeat_enabled` | 每日心跳开关 | `false` |
| `notify.heartbeat_hour` | 每日心跳小时（0-23） | `9` |

完整配置请直接参考：`config.toml.example`。

### 环境变量示例

```bash
UPM_USERNAME=2023xxxxxxx
UPM_PASSWORD=your_password
UPM_DATABASE_URL=sqlite://data/power_monitor.db
UPM_TIMEZONE=Asia/Shanghai
UPM_NOTIFY__ENABLED=true
UPM_NOTIFY__NOTIFY_TYPES=telegram,ntfy,email
```

`notify` 子项使用 `__` 分隔层级（例如 `UPM_NOTIFY__THRESHOLD`）。

### Docker Secrets 支持

可选 secrets（存在即读取）：

- `/run/secrets/username`
- `/run/secrets/password`
- `/run/secrets/service_url`（当前代码中预留）
- `/run/secrets/database_url`

---

## 通知系统

### 事件类型

- **LowBalance**：余额低于阈值时触发（支持冷却与边沿触发逻辑）
- **Heartbeat**：每天指定小时发送一次状态心跳
- **LoginFailure**：启动登录失败时发送
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

---

## 常见问题

### 1）启动时报登录失败

- 检查学号/密码是否正确
- 检查网络是否可访问 UESTC 服务
- 若使用 `wechat` 登录，确认对应登录流程可用

### 2）没有收到通知

- 确认 `notify.enabled = true`
- 确认通道参数完整（如 Telegram token/chat_id）
- 低余额通知受阈值与冷却时间影响

### 3）Webhook/ntfy URL 被拒绝

- 需使用公网 `https` 地址
- 不可指向 localhost/内网地址或解析到内网 IP

### 4）本地构建提示 `../uestc-client` 路径问题

`Cargo.toml` 中包含开发期本地补丁：

```toml
[patch.crates-io]
uestc-client = { path = "../uestc-client" }
```

若你没有该同级目录，请移除此段，改用 crates.io 版本。

---

## License

MIT
