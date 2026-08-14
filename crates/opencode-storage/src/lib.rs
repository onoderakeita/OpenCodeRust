pub mod database;
pub mod repository;
pub mod schema;

pub use database::*;
pub use repository::{
    InputRepository, MessageRepository, SessionRepository, TodoItem, TodoRepository,
};
pub use schema::*;

use anyhow::Result;
use sqlx::{sqlite::SqlitePool, Pool, Sqlite};

pub struct Storage {
    pool: Pool<Sqlite>,
}

impl Storage {
    pub async fn new(db_path: &str) -> Result<Self> {
        let pool = SqlitePool::connect(&format!("sqlite:{}", db_path)).await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
