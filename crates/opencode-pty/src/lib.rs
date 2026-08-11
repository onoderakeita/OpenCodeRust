use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use tokio::sync::mpsc;

pub struct PtySession {
    pub pty_pair: portable_pty::PtyPair,
    pub writer: Box<dyn Write + Send>,
}

pub struct PtyManager {
    pty_system: Box<dyn PtySystem + Send>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            pty_system: Box::new(NativePtySystem::default()),
        }
    }

    pub fn spawn(&self, command: &str, args: Vec<String>) -> anyhow::Result<PtySession> {
        let pty_pair = self.pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        pty_pair.slave.spawn_command(cmd)?;

        let writer = pty_pair.master.take_writer()?;

        Ok(PtySession {
            pty_pair,
            writer,
        })
    }
}

impl PtySession {
    pub fn stream_output(&self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(100);
        let mut reader = self.pty_pair.master.try_clone_reader().unwrap();

        tokio::task::spawn_blocking(move || {
            let mut buffer = [0u8; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(n) if n > 0 => {
                        let output = String::from_utf8_lossy(&buffer[..n]).to_string();
                        if tx.blocking_send(output).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pty_streaming() {
        let manager = PtyManager::new();
        let session = manager.spawn("echo", vec!["hello".to_string()]).unwrap();
        
        let mut rx = session.stream_output();
        let output = rx.recv().await.unwrap();
        
        assert!(output.trim().contains("hello"));
    }
}

