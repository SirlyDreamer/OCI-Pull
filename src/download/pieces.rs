use super::{Progress, remove_file, segment_path};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, io::Write, path::Path, sync::Mutex};

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) struct Piece {
    pub start: u64,
    pub end: u64,
}
#[derive(Serialize, Deserialize)]
struct Layout {
    version: u8,
    size: u64,
    pieces: Vec<Piece>,
}
enum Work {
    Resume(usize),
    Gap(Piece),
}
pub(super) struct Pieces {
    size: u64,
    pub entries: Vec<Piece>,
    pending: VecDeque<Work>,
}
impl Pieces {
    pub async fn load(directory: &Path, size: u64) -> Result<(Self, Vec<u64>)> {
        tokio::fs::create_dir_all(directory).await?;
        let mut entries = match tokio::fs::read(directory.join("layout.json")).await {
            Ok(data) => {
                let layout: Layout = serde_json::from_slice(&data)?;
                ensure!(
                    layout.version == 1 && layout.size == size,
                    "分块布局与镜像层不匹配"
                );
                layout.pieces
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        entries.sort_by_key(|piece| piece.start);
        let mut end = 0;
        let mut kept = Vec::new();
        let mut values = Vec::new();
        for piece in entries {
            ensure!(
                piece.start >= end && piece.start <= piece.end && piece.end < size,
                "分块范围重叠或越界"
            );
            end = piece.end + 1;
            let path = segment_path(directory, piece.start, piece.end);
            let length = match tokio::fs::metadata(&path).await {
                Ok(m) => m.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                Err(e) => return Err(e.into()),
            };
            if length > piece.end - piece.start + 1 {
                remove_file(&path).await?;
            }
            if length > 0 && length <= piece.end - piece.start + 1 {
                kept.push(piece);
                values.push(length);
            } else {
                remove_file(&path).await?;
            }
        }
        let mut pending = VecDeque::new();
        let mut start = 0;
        for (i, piece) in kept.iter().enumerate() {
            if start < piece.start {
                pending.push_back(Work::Gap(Piece {
                    start,
                    end: piece.start - 1,
                }));
            }
            if values[i] < piece.end - piece.start + 1 {
                pending.push_back(Work::Resume(i));
            }
            start = piece.end + 1;
        }
        if start < size {
            pending.push_back(Work::Gap(Piece {
                start,
                end: size - 1,
            }));
        }
        let state = Self {
            size,
            entries: kept,
            pending,
        };
        state.persist(directory).await?;
        Ok((state, values))
    }
    pub fn has_work(&self) -> bool {
        !self.pending.is_empty()
    }
    // Claim only after receiving a task slot, so live block-size changes apply
    // to the very next new piece instead of a pre-expanded static work list.
    pub async fn claim(
        &mut self,
        directory: &Path,
        chunk_size: u64,
        progress: &Mutex<Progress>,
    ) -> Result<Option<(usize, Piece)>> {
        let Some(work) = self.pending.pop_front() else {
            return Ok(None);
        };
        match work {
            Work::Resume(index) => Ok(Some((index, self.entries[index]))),
            Work::Gap(gap) => {
                let piece = Piece {
                    start: gap.start,
                    end: (gap.start + chunk_size - 1).min(gap.end),
                };
                if piece.end < gap.end {
                    self.pending.push_front(Work::Gap(Piece {
                        start: piece.end + 1,
                        end: gap.end,
                    }));
                }
                let index = self.entries.len();
                self.entries.push(piece);
                self.persist(directory).await?;
                progress.lock().unwrap().values.push(0);
                Ok(Some((index, piece)))
            }
        }
    }
    async fn persist(&self, directory: &Path) -> Result<()> {
        let data = serde_json::to_vec(&Layout {
            version: 1,
            size: self.size,
            pieces: self.entries.clone(),
        })?;
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut file = tempfile::NamedTempFile::new_in(&directory)?;
            file.write_all(&data)?;
            file.as_file().sync_all()?;
            file.persist(directory.join("layout.json"))
                .map_err(|e| e.error)?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}
fn parse_name(name: &str, size: u64) -> Option<Piece> {
    let (start, end) = name.strip_suffix(".part")?.split_once('-')?;
    let piece = Piece {
        start: start.parse().ok()?,
        end: end.parse().ok()?,
    };
    (piece.start <= piece.end && piece.end < size).then_some(piece)
}
pub(super) async fn clear(directory: &Path, size: u64) -> Result<()> {
    let mut files = match tokio::fs::read_dir(directory).await {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    while let Some(file) = files.next_entry().await? {
        if let Some(piece) = parse_name(&file.file_name().to_string_lossy(), size) {
            remove_file(&segment_path(directory, piece.start, piece.end)).await?;
        }
    }
    drop(files);
    remove_file(&directory.join("layout.json")).await?;
    match tokio::fs::remove_dir(directory).await {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
