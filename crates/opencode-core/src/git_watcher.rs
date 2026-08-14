use crate::git::Repository;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GitEvent {
    BranchChanged,
    CommitMade,
    IndexChanged,
}

pub struct GitWatcher {
    _repo: Repository,
    event_tx: broadcast::Sender<GitEvent>,
    _watcher: RecommendedWatcher,
}

impl GitWatcher {
    pub fn new(repo: Repository) -> anyhow::Result<Self> {
        let (event_tx, _) = broadcast::channel(100);
        let event_tx_clone = event_tx.clone();

        // Watch .git directory
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(_event) = res {
                // Simplified: notify on any change in .git
                let _ = event_tx_clone.send(GitEvent::IndexChanged);
            }
        })?;

        watcher.watch(&repo.git_directory, RecursiveMode::Recursive)?;

        Ok(Self {
            _repo: repo,
            event_tx,
            _watcher: watcher,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GitEvent> {
        self.event_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn test_git_watcher() {
        let root = tempdir().unwrap();
        let worktree = root.path().to_path_buf();
        let git_dir = worktree.join(".git");

        StdCommand::new("git")
            .arg("init")
            .current_dir(&worktree)
            .status()
            .unwrap();

        let repo = Repository {
            worktree: worktree.clone(),
            git_directory: git_dir.clone(),
            common_directory: git_dir.clone(),
        };
        let watcher = GitWatcher::new(repo).unwrap();
        let mut rx = watcher.subscribe();

        // Trigger a git change
        StdCommand::new("git")
            .arg("config")
            .arg("user.email")
            .arg("test@example.com")
            .current_dir(&worktree)
            .status()
            .unwrap();

        // Wait for event
        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event, GitEvent::IndexChanged);
    }
}
