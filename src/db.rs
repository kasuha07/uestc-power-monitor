use crate::config::AppConfig;
use std::sync::Arc;

pub struct DbService {
    config: Arc<AppConfig>,
}

impl DbService {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }

    pub async fn init(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Example usage
        println!(
            "Initializing DB with config for user: {}",
            self.config.username
        );
        Ok(())
    }

    pub async fn save_data(&self, _data: &()) -> Result<(), Box<dyn std::error::Error>> {
        println!("Saving data to database...");
        Ok(())
    }
}
