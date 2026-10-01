use crate::api::PowerInfo;
use crate::time;
use futures_util::{Stream, StreamExt};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Pool, Sqlite};
use std::path::Path;
use tracing::{debug, info};

#[derive(Clone)]
pub struct DbService {
    pool: Pool<Sqlite>,
}

#[derive(serde::Serialize)]
pub struct PowerRecord {
    pub remaining_money: f64,
    pub remaining_energy: f64,
    pub room_display_name: String,
    pub created_at: String,
}

impl DbService {
    pub async fn new(database_url: String) -> Result<Self, Box<dyn std::error::Error>> {
        debug!("Creating database connection pool for: {}", database_url);

        // Extract file path from database URL and ensure parent directory exists
        if let Some(path) = database_url.strip_prefix("sqlite://") {
            let db_path = Path::new(path);
            if let Some(parent) = db_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                    debug!("Ensured database directory exists: {:?}", parent);
                }
            }
        }

        // Add create_if_missing option to connection string
        let connection_url = if database_url.contains('?') {
            format!("{}&mode=rwc", database_url)
        } else {
            format!("{}?mode=rwc", database_url)
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&connection_url)
            .await?;
        debug!("Database connection pool created successfully");

        Ok(Self { pool })
    }

    pub async fn init(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Initializing DB...");

        // Enable WAL mode for better performance
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&self.pool)
            .await?;
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(&self.pool)
            .await?;

        debug!("Creating power_records table if not exists...");

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS power_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                remaining_energy REAL NOT NULL,
                remaining_money REAL NOT NULL,
                meter_room_id TEXT NOT NULL,
                room_display_name TEXT NOT NULL,
                room_id TEXT NOT NULL,
                building_id TEXT NOT NULL,
                campus_id TEXT NOT NULL,
                room_number TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        // RFC3339 offsets can change after a timezone reload; index actual instants.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS power_records_sampled_at \
             ON power_records (julianday(created_at), id)",
        )
        .execute(&self.pool)
        .await?;

        debug!("Database initialization completed");
        Ok(())
    }

    pub async fn save_data(&self, data: &PowerInfo) -> Result<(), Box<dyn std::error::Error>> {
        debug!(
            "Saving data to database: room={}, money={:.2}, energy={:.2}",
            data.room_display_name, data.remaining_money, data.remaining_energy
        );

        sqlx::query(
            r#"
            INSERT INTO power_records (
                remaining_energy, remaining_money, meter_room_id,
                room_display_name, room_id, building_id, campus_id, room_number, created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(data.remaining_energy)
        .bind(data.remaining_money)
        .bind(&data.meter_room_id)
        .bind(&data.room_display_name)
        .bind(&data.room_id)
        .bind(&data.building_id)
        .bind(&data.campus_id)
        .bind(&data.room_number)
        .bind(time::now_rfc3339())
        .execute(&self.pool)
        .await?;

        debug!("Data saved successfully to database");
        Ok(())
    }

    pub async fn recent_records(&self, limit: u32) -> Result<Vec<PowerRecord>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (f64, f64, String, String)>(
            "SELECT remaining_money, remaining_energy, room_display_name, created_at \
             FROM power_records ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(remaining_money, remaining_energy, room_display_name, created_at)| PowerRecord {
                    remaining_money,
                    remaining_energy,
                    room_display_name,
                    created_at,
                },
            )
            .collect())
    }

    /// 流式读取完整趋势窗口及窗口前最后一条基线，不按采样条数截断。
    /// 按实际时刻排序，兼容历史数据含不同时区偏移；调用方逐条聚合以限制内存。
    pub fn trend_records(
        &self,
        since: &str,
    ) -> impl Stream<Item = Result<PowerRecord, sqlx::Error>> + '_ {
        sqlx::query_as::<_, (f64, f64, String, String)>(
            "WITH baseline AS ( \
                SELECT id, remaining_money, remaining_energy, room_display_name, created_at \
                FROM power_records WHERE julianday(created_at) < julianday(?1) \
                ORDER BY julianday(created_at) DESC, id DESC LIMIT 1 \
             ), samples AS ( \
                SELECT * FROM baseline UNION ALL \
                SELECT id, remaining_money, remaining_energy, room_display_name, created_at \
                FROM power_records WHERE julianday(created_at) >= julianday(?1) \
             ) \
             SELECT remaining_money, remaining_energy, room_display_name, created_at \
             FROM samples ORDER BY julianday(created_at), id",
        )
        .bind(since.to_owned())
        .fetch(&self.pool)
        .map(|row| {
            row.map(
                |(remaining_money, remaining_energy, room_display_name, created_at)| PowerRecord {
                    remaining_money,
                    remaining_energy,
                    room_display_name,
                    created_at,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use futures_util::TryStreamExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn sample_power_info() -> PowerInfo {
        PowerInfo {
            code: 0,
            message: "ok".to_string(),
            remaining_energy: 10.0,
            remaining_money: 20.0,
            meter_room_id: "meter-room-id".to_string(),
            room_display_name: "220407".to_string(),
            room_id: "room-id".to_string(),
            building_id: "building-id".to_string(),
            campus_id: "campus-id".to_string(),
            room_number: "407".to_string(),
        }
    }

    #[tokio::test]
    async fn trend_records_order_instants_and_include_only_the_latest_baseline() {
        let service = DbService::new("sqlite::memory:".into()).await.unwrap();
        service.init().await.unwrap();
        for (at, money) in [
            ("2026-09-20T20:00:00+08:00", 20.0),
            ("2026-09-20T13:00:00+00:00", 18.0),
            ("2026-09-20T22:00:00+08:00", 16.0),
            ("2026-09-20T11:00:00+00:00", 22.0),
        ] {
            sqlx::query(
                "INSERT INTO power_records (remaining_energy, remaining_money, meter_room_id, \
                    room_display_name, room_id, building_id, campus_id, room_number, created_at) \
                 VALUES (10, ?, 'm', 'room', 'r', 'b', 'c', '1', ?)",
            )
            .bind(money)
            .bind(at)
            .execute(&service.pool)
            .await
            .unwrap();
        }
        let records: Vec<_> = service
            .trend_records("2026-09-20T12:30:00+00:00")
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.remaining_money)
                .collect::<Vec<_>>(),
            vec![20.0, 18.0, 16.0]
        );
        service.pool.close().await;
    }

    #[tokio::test]
    async fn save_data_stores_rfc3339_created_at_with_offset() {
        let _guard = crate::time::TEST_TIME_MUTEX
            .lock()
            .expect("lock timezone test mutex");
        let original_timezone = crate::time::current_timezone_name();
        crate::time::set_timezone("Asia/Shanghai").expect("set test timezone");

        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let db_path = std::env::temp_dir().join(format!("upm-created-at-{uniq}.db"));
        let database_url = format!("sqlite://{}", db_path.display());

        let service = DbService::new(database_url.clone())
            .await
            .expect("create db service");
        service.init().await.expect("init db");
        service
            .save_data(&sample_power_info())
            .await
            .expect("save power info");

        let read_url = format!("{}?mode=ro", database_url);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&read_url)
            .await
            .expect("connect read pool");
        let created_at: String =
            sqlx::query_scalar("SELECT created_at FROM power_records ORDER BY id DESC LIMIT 1")
                .fetch_one(&pool)
                .await
                .expect("read created_at");

        let parsed =
            DateTime::parse_from_rfc3339(&created_at).expect("created_at should be RFC3339");
        assert_eq!(parsed.offset().local_minus_utc(), 8 * 3600);

        drop(pool);
        drop(service);
        let _ = std::fs::remove_file(db_path);
        crate::time::set_timezone(&original_timezone).expect("restore timezone");
    }
}
