use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EntryType {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub entry_type: EntryType,
}

#[async_trait::async_trait]
pub trait FileSystem {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>>;
    async fn list(&self, path: &Path) -> anyhow::Result<Vec<Entry>>;
}

pub struct FileSystemImpl {
    root: PathBuf,
}

impl FileSystemImpl {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait::async_trait]
impl FileSystem for FileSystemImpl {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        let full_path = self.root.join(path);
        let content = fs::read(full_path).await?;
        Ok(content)
    }

    async fn list(&self, path: &Path) -> anyhow::Result<Vec<Entry>> {
        let full_path = self.root.join(path);
        let mut entries = Vec::new();
        let mut dir = fs::read_dir(full_path).await?;
        
        while let Some(entry) = dir.next_entry().await? {
            let file_type = entry.file_type().await?;
            let entry_type = if file_type.is_dir() {
                EntryType::Directory
            } else {
                EntryType::File
            };
            entries.push(Entry {
                path: entry.path().strip_prefix(&self.root)?.to_path_buf(),
                entry_type,
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_fs_read_list() {
        let root = tempdir().unwrap();
        let fs_impl = FileSystemImpl::new(root.path().to_path_buf());
        
        // Create file
        fs::write(root.path().join("test.txt"), b"hello").await.unwrap();
        
        // Read file
        let content = fs_impl.read(Path::new("test.txt")).await.unwrap();
        assert_eq!(content, b"hello");
        
        // List directory
        let entries = fs_impl.list(Path::new("")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("test.txt"));
        assert_eq!(entries[0].entry_type, EntryType::File);
    }
}
