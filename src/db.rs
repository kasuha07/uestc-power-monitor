use crate::api::PowerInfo;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Pool, Sqlite};
use std::path::Path;
use tracing::{debug, info};

pub struct DbService {
    pool: Pool<Sqlite>,
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
        sqlx::query("PRAGMA journal_mode=WAL").execute(&self.pool).await?;
        sqlx::query("PRAGMA synchronous=NORMAL").execute(&self.pool).await?;

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
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        debug!("Database initialization completed");
        Ok(())
    }

    pub async fn save_data(&self, data: &PowerInfo) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Saving data to database: room={}, money={:.2}, energy={:.2}",
            data.room_display_name, data.remaining_money, data.remaining_energy);

        sqlx::query(
            r#"
            INSERT INTO power_records (
                remaining_energy, remaining_money, meter_room_id,
                room_display_name, room_id, building_id, campus_id, room_number
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
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
        .execute(&self.pool)
        .await?;

        debug!("Data saved successfully to database");
        Ok(())
    }
}
