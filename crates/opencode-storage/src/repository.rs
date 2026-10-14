use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json;
use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;

use opencode_types::{MessagePart, MessageRole, Session, SessionMessage, SessionStatus};

use crate::database::DatabaseError;

#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct SessionRow {
    id: String,
    project_id: String,
    parent_id: Option<String>,
    slug: String,
    directory: String,
    title: String,
    version: String,
    share_url: Option<String>,
    summary_additions: Option<i64>,
    summary_deletions: Option<i64>,
    summary_files: Option<i64>,
    summary_diffs: Option<String>,
    revert: Option<String>,
    permission: Option<String>,
    usage_input_tokens: Option<i64>,
    usage_output_tokens: Option<i64>,
    usage_reasoning_tokens: Option<i64>,
    usage_cache_write_tokens: Option<i64>,
    usage_cache_read_tokens: Option<i64>,
    usage_total_cost: Option<f64>,
    status: SessionStatus,
    created_at: i64,
    updated_at: i64,
    time_compacting: Option<i64>,
    time_archived: Option<i64>,
}

impl SessionRow {
    fn into_session(self) -> Session {
        Session {
            id: opencode_types::SessionID(self.id),
            slug: self.slug,
            project_id: opencode_types::ProjectID(self.project_id),
            directory: self.directory,
            parent_id: self.parent_id.map(opencode_types::SessionID),
            title: self.title,
            version: self.version.to_string(),
            time: opencode_types::TimeInfo {
                created: self.created_at as u64,
                updated: self.updated_at as u64,
                compacting: self.time_compacting.map(|t| t as u64),
                archived: self.time_archived.map(|t| t as u64),
            },
            messages: Vec::new(),
            summary: None,
            share: self
                .share_url
                .map(|url| opencode_types::SessionShareInfo { url }),
            revert: self.revert.and_then(|s| serde_json::from_str(&s).ok()),
            permission: self.permission.and_then(|p| serde_json::from_str(&p).ok()),
            usage: Some(opencode_types::TokenUsage {
                input: self.usage_input_tokens.unwrap_or(0) as f64,
                output: self.usage_output_tokens.unwrap_or(0) as f64,
                reasoning: self.usage_reasoning_tokens.unwrap_or(0) as f64,
                cache: opencode_types::CacheUsage {
                    read: self.usage_cache_read_tokens.unwrap_or(0) as f64,
                    write: self.usage_cache_write_tokens.unwrap_or(0) as f64,
                },
            }),
            status: self.status,
            metadata: HashMap::new(),
            created_at: chrono::DateTime::from_timestamp_millis(self.created_at)
                .unwrap_or_else(chrono::Utc::now),
            updated_at: chrono::DateTime::from_timestamp_millis(self.updated_at)
                .unwrap_or_else(chrono::Utc::now),
        }
    }
}

pub struct SessionRepository {
    pool: SqlitePool,
}

impl SessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, session: &Session) -> Result<(), DatabaseError> {
        let summary_diffs = session
            .summary
            .as_ref()
            .and_then(|s| serde_json::to_string(&s.diffs).ok());

        let revert_json = session.revert.as_ref().map(sqlx::types::Json);
        let permission_json = session.permission.as_ref().map(sqlx::types::Json);
        let share_url = session.share.as_ref().map(|s| s.url.clone());

        sqlx::query(
            r#"
            INSERT INTO sessions (
                id, project_id, parent_id, slug, directory, title, version, share_url,
                summary_additions, summary_deletions, summary_files, summary_diffs,
                revert, permission,
                usage_input_tokens, usage_output_tokens, usage_reasoning_tokens,
                usage_cache_write_tokens, usage_cache_read_tokens, usage_total_cost,
                status, created_at, updated_at, time_compacting, time_archived
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(session.id.0.clone())
        .bind(session.project_id.0.clone())
        .bind(session.parent_id.as_ref().map(|id| id.0.clone()))
        .bind(&session.slug)
        .bind(&session.directory)
        .bind(&session.title)
        .bind(&session.version)
        .bind(share_url)
        .bind(session.summary.as_ref().map(|s| s.additions as i64))
        .bind(session.summary.as_ref().map(|s| s.deletions as i64))
        .bind(session.summary.as_ref().map(|s| s.files as i64))
        .bind(summary_diffs)
        .bind(revert_json.map(sqlx::types::Json))
        .bind(permission_json.map(sqlx::types::Json))
        .bind(session.usage.as_ref().map(|u| u.input).unwrap_or(0.0))
        .bind(session.usage.as_ref().map(|u| u.output).unwrap_or(0.0))
        .bind(session.usage.as_ref().map(|u| u.reasoning).unwrap_or(0.0))
        .bind(session.usage.as_ref().map(|u| u.cache.write).unwrap_or(0.0))
        .bind(session.usage.as_ref().map(|u| u.cache.read).unwrap_or(0.0))
        .bind(
            session
                .usage
                .as_ref()
                .map(|u| u.input + u.output)
                .unwrap_or(0.0),
        ) // Placeholder for total cost
        .bind(&session.status)
        .bind(session.created_at.timestamp_millis())
        .bind(session.updated_at.timestamp_millis())
        .bind(session.time.compacting.map(|t| t as i64))
        .bind(session.time.archived.map(|t| t as i64))
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Session>, DatabaseError> {
        let row = sqlx::query_as::<_, SessionRow>(
            r#"SELECT 
                id, project_id, parent_id, slug, directory, title, version, share_url,
                summary_additions, summary_deletions, summary_files, summary_diffs,
                revert, permission,
                usage_input_tokens, usage_output_tokens, usage_reasoning_tokens,
                usage_cache_write_tokens, usage_cache_read_tokens, usage_total_cost,
                status, created_at, updated_at, time_compacting, time_archived
            FROM sessions WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(row.map(|r| r.into_session()))
    }

    pub async fn list(
        &self,
        project_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Session>, DatabaseError> {
        let rows = match project_id {
            Some(pid) => sqlx::query_as::<_, SessionRow>(
                r#"SELECT 
                        id, project_id, parent_id, slug, directory, title, version, share_url,
                        summary_additions, summary_deletions, summary_files, summary_diffs,
                        revert, permission,
                        usage_input_tokens, usage_output_tokens, usage_reasoning_tokens,
                        usage_cache_write_tokens, usage_cache_read_tokens, usage_total_cost,
                        status, created_at, updated_at, time_compacting, time_archived
                    FROM sessions WHERE project_id = ? 
                    ORDER BY updated_at DESC LIMIT ?"#,
            )
            .bind(pid)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?,
            None => sqlx::query_as::<_, SessionRow>(
                r#"SELECT 
                        id, project_id, parent_id, slug, directory, title, version, share_url,
                        summary_additions, summary_deletions, summary_files, summary_diffs,
                        revert, permission,
                        usage_input_tokens, usage_output_tokens, usage_reasoning_tokens,
                        usage_cache_write_tokens, usage_cache_read_tokens, usage_total_cost,
                        status, created_at, updated_at, time_compacting, time_archived
                    FROM sessions 
                    ORDER BY updated_at DESC LIMIT ?"#,
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?,
        };

        Ok(rows.into_iter().map(|r| r.into_session()).collect())
    }

    pub async fn update(&self, session: &Session) -> Result<(), DatabaseError> {
        let summary_diffs = session
            .summary
            .as_ref()
            .and_then(|s| serde_json::to_string(&s.diffs).ok());

        let revert_json = session.revert.clone();
        let permission_json = session
            .permission
            .as_ref()
            .and_then(|p| serde_json::to_string(p).ok());

        let share_url = session.share.as_ref().map(|s| s.url.clone());
        let usage = session.usage.as_ref();

        sqlx::query(
            r#"
            UPDATE sessions SET
                title = ?, version = ?, share_url = ?,
                summary_additions = ?, summary_deletions = ?, summary_files = ?, summary_diffs = ?,
                revert = ?, permission = ?,
                usage_input_tokens = ?, usage_output_tokens = ?, usage_reasoning_tokens = ?,
                usage_cache_write_tokens = ?, usage_cache_read_tokens = ?, usage_total_cost = ?,
                status = ?, updated_at = ?, time_compacting = ?, time_archived = ?
            WHERE id = ?
            "#,
        )
        .bind(&session.title)
        .bind(&session.version)
        .bind(share_url)
        .bind(session.summary.as_ref().map(|s| s.additions as i64))
        .bind(session.summary.as_ref().map(|s| s.deletions as i64))
        .bind(session.summary.as_ref().map(|s| s.files as i64))
        .bind(summary_diffs)
        .bind(revert_json.map(sqlx::types::Json))
        .bind(permission_json.map(sqlx::types::Json))
        .bind(usage.map(|u| u.input).unwrap_or(0.0))
        .bind(usage.map(|u| u.output).unwrap_or(0.0))
        .bind(usage.map(|u| u.reasoning).unwrap_or(0.0))
        .bind(usage.map(|u| u.cache.write).unwrap_or(0.0))
        .bind(usage.map(|u| u.cache.read).unwrap_or(0.0))
        .bind(usage.map(|u| u.input + u.output).unwrap_or(0.0))
        .bind(&session.status)
        .bind(session.updated_at.timestamp_millis())
        .bind(session.time.compacting.map(|t| t as i64))
        .bind(session.time.archived.map(|t| t as i64))
        .bind(&session.id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<(), DatabaseError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn list_children(&self, parent_id: &str) -> Result<Vec<Session>, DatabaseError> {
        let rows = sqlx::query_as::<_, SessionRow>(
            r#"SELECT 
                id, project_id, parent_id, slug, directory, title, version, share_url,
                summary_additions, summary_deletions, summary_files, summary_diffs,
                revert, permission,
                usage_input_tokens, usage_output_tokens, usage_reasoning_tokens,
                usage_cache_write_tokens, usage_cache_read_tokens, usage_total_cost,
                status, created_at, updated_at, time_compacting, time_archived
            FROM sessions WHERE parent_id = ? 
            ORDER BY created_at DESC"#,
        )
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(rows.into_iter().map(|r| r.into_session()).collect())
    }
}

pub struct MessageRepository {
    pool: SqlitePool,
}

impl MessageRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, message: &SessionMessage) -> Result<(), DatabaseError> {
        let data_json = serde_json::to_string(&message.parts)
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        let role_str = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        };

        sqlx::query(
            r#"
            INSERT INTO messages (id, session_id, role, created_at, data)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(&message.id)
        .bind(&message.session_id)
        .bind(role_str)
        .bind(message.created_at.timestamp_millis())
        .bind(&data_json)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn upsert(&self, message: &SessionMessage) -> Result<(), DatabaseError> {
        let data_json = serde_json::to_string(&message.parts)
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        let role_str = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        };

        sqlx::query(
            r#"
            INSERT INTO messages (id, session_id, role, created_at, data)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                session_id = excluded.session_id,
                role = excluded.role,
                created_at = excluded.created_at,
                data = excluded.data
            "#,
        )
        .bind(&message.id)
        .bind(&message.session_id)
        .bind(role_str)
        .bind(message.created_at.timestamp_millis())
        .bind(&data_json)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn list_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionMessage>, DatabaseError> {
        #[derive(FromRow)]
        struct MessageRow {
            id: String,
            session_id: String,
            role: String,
            created_at: i64,
            data: Option<String>,
        }

        let rows = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, session_id, role, created_at, data 
               FROM messages WHERE session_id = ? ORDER BY created_at ASC"#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        let messages: Vec<SessionMessage> = rows
            .into_iter()
            .filter_map(|row| {
                let msg_role = match row.role.as_str() {
                    "user" => MessageRole::User,
                    "assistant" => MessageRole::Assistant,
                    "system" => MessageRole::System,
                    "tool" => MessageRole::Tool,
                    _ => return None,
                };

                let parts: Vec<MessagePart> = row
                    .data
                    .and_then(|c| serde_json::from_str(&c).ok())
                    .unwrap_or_default();

                let created =
                    DateTime::from_timestamp_millis(row.created_at).unwrap_or_else(Utc::now);

                Some(SessionMessage {
                    id: row.id,
                    session_id: row.session_id,
                    role: msg_role,
                    parts,
                    created_at: created,
                    metadata: HashMap::new(),
                })
            })
            .collect();

        Ok(messages)
    }

    pub async fn get(&self, id: &str) -> Result<Option<SessionMessage>, DatabaseError> {
        #[derive(FromRow)]
        struct MessageRow {
            id: String,
            session_id: String,
            role: String,
            created_at: i64,
            data: Option<String>,
        }

        let row = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, session_id, role, created_at, data 
               FROM messages WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        match row {
            Some(row) => {
                let msg_role = match row.role.as_str() {
                    "user" => MessageRole::User,
                    "assistant" => MessageRole::Assistant,
                    "system" => MessageRole::System,
                    "tool" => MessageRole::Tool,
                    _ => return Ok(None),
                };

                let parts: Vec<MessagePart> = row
                    .data
                    .and_then(|c| serde_json::from_str(&c).ok())
                    .unwrap_or_default();

                let created =
                    DateTime::from_timestamp_millis(row.created_at).unwrap_or_else(Utc::now);

                Ok(Some(SessionMessage {
                    id: row.id,
                    session_id: row.session_id,
                    role: msg_role,
                    parts,
                    created_at: created,
                    metadata: HashMap::new(),
                }))
            }
            None => Ok(None),
        }
    }

    pub async fn delete(&self, id: &str) -> Result<(), DatabaseError> {
        sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn delete_for_session(&self, session_id: &str) -> Result<(), DatabaseError> {
        sqlx::query("DELETE FROM messages WHERE session_id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
    pub priority: String,
    pub position: i64,
}

pub struct TodoRepository {
    pool: SqlitePool,
}

impl TodoRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list_for_session(&self, session_id: &str) -> Result<Vec<TodoItem>, DatabaseError> {
        #[derive(FromRow)]
        struct TodoRow {
            todo_id: String,
            content: String,
            status: String,
            priority: String,
            position: i64,
        }

        let rows = sqlx::query_as::<_, TodoRow>(
            r#"SELECT todo_id, content, status, priority, position 
               FROM todos WHERE session_id = ? ORDER BY position ASC"#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|row| TodoItem {
                id: row.todo_id,
                content: row.content,
                status: row.status,
                priority: row.priority,
                position: row.position,
            })
            .collect())
    }

    pub async fn upsert(&self, session_id: &str, todo: &TodoItem) -> Result<(), DatabaseError> {
        let now = Utc::now().timestamp_millis();

        sqlx::query(
            r#"
            INSERT INTO todos (session_id, todo_id, content, status, priority, position, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(session_id, todo_id) DO UPDATE SET
                content = excluded.content,
                status = excluded.status,
                priority = excluded.priority,
                position = excluded.position,
                updated_at = excluded.updated_at
            "#
        )
        .bind(session_id)
        .bind(&todo.id)
        .bind(&todo.content)
        .bind(&todo.status)
        .bind(&todo.priority)
        .bind(todo.position)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn delete(&self, session_id: &str, todo_id: &str) -> Result<(), DatabaseError> {
        sqlx::query("DELETE FROM todos WHERE session_id = ? AND todo_id = ?")
            .bind(session_id)
            .bind(todo_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn delete_for_session(&self, session_id: &str) -> Result<(), DatabaseError> {
        sqlx::query("DELETE FROM todos WHERE session_id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }
}

#[derive(Clone)]
pub struct InputRepository {
    pool: SqlitePool,
}

impl InputRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn enqueue(
        &self,
        session_id: &str,
        prompt: &str,
        delivery: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Result<opencode_types::SessionInput, DatabaseError> {
        let created_at = chrono::Utc::now().timestamp_millis();
        let id = format!("in_{}_{}", created_at, uuid::Uuid::new_v4());
        let metadata_json = metadata
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;
        let returned_metadata = metadata.cloned();
        sqlx::query(
            "INSERT INTO session_input (id, session_id, prompt, delivery, metadata, status, created_at) VALUES (?, ?, ?, ?, ?, 'pending', ?)",
        )
        .bind(&id)
        .bind(session_id)
        .bind(prompt)
        .bind(delivery)
        .bind(metadata_json)
        .bind(created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(opencode_types::SessionInput {
            id,
            session_id: session_id.to_owned(),
            prompt: prompt.to_owned(),
            delivery: delivery.map(str::to_owned),
            metadata: returned_metadata,
            status: "pending".to_owned(),
            created_at: created_at as u64,
        })
    }

    pub async fn list_pending_all(
        &self,
    ) -> Result<Vec<opencode_types::SessionInput>, DatabaseError> {
        #[derive(FromRow)]
        struct InputRow {
            id: String,
            session_id: String,
            prompt: String,
            delivery: Option<String>,
            metadata: Option<String>,
            status: String,
            created_at: i64,
        }

        let rows = sqlx::query_as::<_, InputRow>(
            "SELECT id, session_id, prompt, delivery, metadata, status, created_at FROM session_input WHERE status = 'pending' ORDER BY created_at ASC, rowid ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|row| opencode_types::SessionInput {
                id: row.id,
                session_id: row.session_id,
                prompt: row.prompt,
                delivery: row.delivery,
                metadata: row
                    .metadata
                    .and_then(|value| serde_json::from_str(&value).ok()),
                status: row.status,
                created_at: row.created_at as u64,
            })
            .collect())
    }

    pub async fn claim_pending(&self, id: &str) -> Result<bool, DatabaseError> {
        let result = sqlx::query(
            r#"UPDATE session_input SET status = 'processing'
               WHERE id = ? AND status = 'pending'
                 AND NOT EXISTS (
                     SELECT 1 FROM session_input earlier
                     WHERE earlier.session_id = session_input.session_id
                       AND earlier.status IN ('pending', 'processing')
                       AND (
                           earlier.created_at < session_input.created_at
                           OR (earlier.created_at = session_input.created_at AND earlier.rowid < session_input.rowid)
                       )
                 )"#,
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn release_for_retry(&self, id: &str) -> Result<(), DatabaseError> {
        sqlx::query(
            "UPDATE session_input SET status = 'pending' WHERE id = ? AND status = 'processing'",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;
        Ok(())
    }

    pub async fn list_pending(
        &self,
        session_id: &str,
    ) -> Result<Vec<opencode_types::SessionInput>, DatabaseError> {
        #[derive(FromRow)]
        struct InputRow {
            id: String,
            session_id: String,
            prompt: String,
            delivery: Option<String>,
            metadata: Option<String>,
            status: String,
            created_at: i64,
        }

        let rows = sqlx::query_as::<_, InputRow>(
            r#"SELECT id, session_id, prompt, delivery, metadata, status, created_at
               FROM session_input WHERE session_id = ? AND status = 'pending' ORDER BY created_at ASC, rowid ASC"#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|row| opencode_types::SessionInput {
                id: row.id,
                session_id: row.session_id,
                prompt: row.prompt,
                delivery: row.delivery,
                metadata: row
                    .metadata
                    .and_then(|value| serde_json::from_str(&value).ok()),
                status: row.status,
                created_at: row.created_at as u64,
            })
            .collect())
    }

    pub async fn mark_as_promoted(&self, id: &str) -> Result<(), DatabaseError> {
        sqlx::query(
            "UPDATE session_input SET status = 'promoted' WHERE id = ? AND status IN ('pending', 'processing')",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    pub async fn mark_as_failed(&self, id: &str) -> Result<(), DatabaseError> {
        sqlx::query(
            "UPDATE session_input SET status = 'failed' WHERE id = ? AND status IN ('pending', 'processing')",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| DatabaseError::QueryError(e.to_string()))?;
        Ok(())
    }

    pub async fn reset_processing(&self) -> Result<(), DatabaseError> {
        sqlx::query("UPDATE session_input SET status = 'pending' WHERE status = 'processing'")
            .execute(&self.pool)
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod input_repository_tests {
    use super::InputRepository;
    use crate::Database;

    #[tokio::test]
    async fn queue_claim_and_restart_recovery_preserve_fifo() {
        let database = Database::in_memory().await.expect("database initializes");
        sqlx::query(
            "INSERT INTO sessions (id, project_id, slug, directory, title, created_at, updated_at) VALUES ('session-1', 'project-1', 'test', '.', 'Test', 1, 1)",
        )
        .execute(database.pool())
        .await
        .expect("session row exists");

        let repository = InputRepository::new(database.pool().clone());
        let first = repository
            .enqueue("session-1", "first", Some("queue"), None)
            .await
            .expect("first input enqueued");
        let second = repository
            .enqueue("session-1", "second", Some("queue"), None)
            .await
            .expect("second input enqueued");

        assert!(repository.claim_pending(&first.id).await.unwrap());
        assert!(!repository.claim_pending(&first.id).await.unwrap());
        assert!(!repository.claim_pending(&second.id).await.unwrap());
        repository.reset_processing().await.unwrap();

        let pending = repository.list_pending("session-1").await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            [first.id.as_str(), second.id.as_str()]
        );
        assert!(repository.claim_pending(&first.id).await.unwrap());
        repository.mark_as_promoted(&first.id).await.unwrap();
        assert!(repository.claim_pending(&second.id).await.unwrap());
    }

    #[tokio::test]
    async fn failed_queue_head_allows_next_item_with_tied_timestamps() {
        let database = Database::in_memory().await.expect("database initializes");
        sqlx::query(
            "INSERT INTO sessions (id, project_id, slug, directory, title, created_at, updated_at) VALUES ('session-1', 'project-1', 'test', '.', 'Test', 1, 1)",
        )
        .execute(database.pool())
        .await
        .expect("session row exists");

        let repository = InputRepository::new(database.pool().clone());
        let first = repository
            .enqueue("session-1", "first", Some("queue"), None)
            .await
            .expect("first input enqueued");
        let second = repository
            .enqueue("session-1", "second", Some("queue"), None)
            .await
            .expect("second input enqueued");

        sqlx::query("UPDATE session_input SET created_at = 42 WHERE id IN (?, ?)")
            .bind(&first.id)
            .bind(&second.id)
            .execute(database.pool())
            .await
            .expect("timestamps are tied");

        let pending = repository.list_pending("session-1").await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            [first.id.as_str(), second.id.as_str()]
        );
        assert!(repository.claim_pending(&first.id).await.unwrap());
        repository.mark_as_failed(&first.id).await.unwrap();
        assert!(repository.claim_pending(&second.id).await.unwrap());
    }
}
