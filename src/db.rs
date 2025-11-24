use crate::api::PowerInfo;
use crate::config::AppConfig;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Pool, Postgres};
use std::sync::Arc;

pub struct DbService {
    config: Arc<AppConfig>,
    pool: Pool<Postgres>,
}

impl DbService {
    pub async fn new(config: Arc<AppConfig>) -> Result<Self, Box<dyn std::error::Error>> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&config.database_url)
            .await?;

        Ok(Self { config, pool })
    }

    pub async fn init(&self) -> Result<(), Box<dyn std::error::Error>> {
        println!("Initializing DB...");

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS power_records (
                id SERIAL PRIMARY KEY,
                remaining_energy FLOAT8 NOT NULL,
                remaining_money FLOAT8 NOT NULL,
                meter_room_id TEXT NOT NULL,
                room_display_name TEXT NOT NULL,
                room_id TEXT NOT NULL,
                building_id TEXT NOT NULL,
                campus_id TEXT NOT NULL,
                room_number TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn save_data(&self, data: &PowerInfo) -> Result<(), Box<dyn std::error::Error>> {
        println!("Saving data to database...");

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

        Ok(())
    }
}
