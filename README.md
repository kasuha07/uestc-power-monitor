# UESTC Power Monitor

电子科技大学（UESTC）宿舍电费监控工具。

本项目旨在自动监控宿舍电费余额，将数据记录到 PostgreSQL 数据库中进行持久化保存，并提供低余额报警功能，避免突然停电的尴尬。

## 功能特性

- 🔌 **自动轮询**: 定时获取电费余额和剩余电量。
- 💾 **数据持久化**: 自动将历史数据保存到 PostgreSQL 数据库，方便后续分析。
- 🚨 **低余额报警**: 当余额低于设定阈值时，自动发送通知。
- 📢 **多渠道通知**: 目前支持 Telegram Bot、Webhook 和控制台输出。

## 快速开始

### 1. 环境准备

- [Rust](https://www.rust-lang.org/tools/install) (编译环境)
- [PostgreSQL](https://www.postgresql.org/) (数据存储)

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

编辑 `config.toml`，填入你的学号、密码以及数据库连接信息。

### 4. 准备数据库

在 PostgreSQL 中创建一个名为 `uestc_power_monitor` 的数据库（或者你在配置文件中指定的名称）。程序启动后会自动创建所需的 `power_records` 表。

```bash
createdb uestc_power_monitor
```

### 5. 编译运行

```bash
# 开发模式运行
cargo run

# 生产模式构建并运行
cargo build --release
./target/release/uestc-power-monitor
```

### 6. Docker 部署

本项目支持 Docker 部署，包含自动构建和数据库配置。

1. **准备配置**: 复制 `config.toml.example` 为 `config.toml` 并填入账号信息。
2. **启动服务**:
   ```bash
   docker-compose up -d --build
   ```

**注意**:
- Docker 部署会自动下载 `uestc-client` 依赖进行构建。
- `docker-compose.yml` 中已预设了数据库连接的环境变量 `UPM_DATABASE_URL`，它会覆盖 `config.toml` 中的数据库设置，确保连接到容器内的数据库。

## 数据表结构

程序会自动创建 `power_records` 表，结构如下：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| id | SERIAL | 主键 |
| remaining_energy | FLOAT8 | 剩余电量 (度) |
| remaining_money | FLOAT8 | 剩余金额 (元) |
| meter_room_id | TEXT | 电表房间ID |
| room_display_name | TEXT | 房间显示名称 |
| created_at | TIMESTAMPTZ | 记录时间 |
| ... | ... | 其他位置信息字段 |

## License

MIT

