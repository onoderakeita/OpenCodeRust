use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use opencode_provider::ProviderRegistry;
use opencode_storage::{InputRepository, MessageRepository, SessionRepository};
use opencode_tool::ToolRegistry;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionState {
    Idle,
    Running,
    Interrupted,
}

#[allow(dead_code)]
pub struct SessionRunner {
    session_id: Uuid,
    state: Arc<Mutex<SessionState>>,
    repo: Arc<SessionRepository>,
    msg_repo: Arc<MessageRepository>,
    input_repo: Arc<InputRepository>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
}

impl SessionRunner {
    pub fn new(
        session_id: Uuid,
        repo: Arc<SessionRepository>,
        msg_repo: Arc<MessageRepository>,
        input_repo: Arc<InputRepository>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            session_id,
            state: Arc::new(Mutex::new(SessionState::Idle)),
            repo,
            msg_repo,
            input_repo,
            providers,
            tools,
        }
    }
    // ... (rest is same)

    pub async fn run(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        if *state == SessionState::Running {
            return Ok(());
        }
        *state = SessionState::Running;

        // 1. 保留中の入力取得
        let pending = self
            .input_repo
            .list_pending(&self.session_id.to_string())
            .await?;

        // 2. エージェントの初期化 (公式仕様に合わせるため、設定の読み込みや AgentExecutor の初期化を行う)
        // ここでは単純化のため、AgentInfo::general() を使用
        let agent = opencode_agent::AgentInfo::general();
        let mut executor =
            opencode_agent::AgentExecutor::new(agent, self.providers.clone(), self.tools.clone());

        for input in pending {
            // 3. プロモート (status = 'promoted')
            self.input_repo.mark_as_promoted(&input.id).await?;

            // 4. LLM 実行
            let response = executor
                .execute(&input.prompt)
                .await
                .map_err(|e| anyhow::anyhow!("Executor error: {}", e))?;

            // 5. 結果を永続化
            let mut assistant_msg =
                opencode_types::SessionMessage::assistant(self.session_id.to_string());
            assistant_msg.parts.push(opencode_types::MessagePart {
                id: format!("prt_{}", uuid::Uuid::new_v4()),
                part_type: opencode_types::PartType::Text {
                    text: response.clone(),
                },
                created_at: chrono::Utc::now(),
                message_id: None,
            });
            self.msg_repo.create(&assistant_msg).await?;

            println!("Response: {}", response);
        }

        *state = SessionState::Idle;
        Ok(())
    }

    pub async fn interrupt(&self) {
        let mut state = self.state.lock().await;
        *state = SessionState::Interrupted;
    }
}
