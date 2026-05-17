use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

#[derive(Parser, Debug)]
#[command(name = "zap")]
#[command(about = "Local Markdown search engine", version)]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    index: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build or rebuild the Markdown index.
    Index {
        #[arg(long, default_value = ".", value_name = "PATH")]
        root: PathBuf,
    },
    /// Search indexed Markdown files for a keyword.
    Search {
        keyword: String,

        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Watch Markdown files and update the index when they change.
    Watch {
        #[arg(long, default_value = ".", value_name = "PATH")]
        root: PathBuf,

        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,

        #[arg(long, value_name = "SECONDS")]
        poll_seconds: Option<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Index {
    version: u32,
    root: PathBuf,
    generated_at: u64,
    files: BTreeMap<PathBuf, IndexedFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexedFile {
    modified: u64,
    size: u64,
    lines: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct FileState {
    modified: u64,
    size: u64,
}

#[derive(Debug)]
struct Match {
    path: PathBuf,
    line_number: usize,
    column: usize,
    line: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let index_path = cli.index.unwrap_or(default_index_path()?);

    match cli.command {
        Command::Index { root } => {
            let index = build_index(&root)?;
            save_index(&index_path, &index)?;
            eprintln!(
                "indexed {} markdown files into {}",
                index.files.len(),
                index_path.display()
            );
        }
        Command::Search { keyword, limit } => {
            let index = load_index(&index_path)?;
            for item in search_index(&index, &keyword, limit) {
                println!(
                    "{}\t{}\t{}\t{}",
                    item.path.display(),
                    item.line_number,
                    item.column,
                    item.line
                );
            }
        }
        Command::Watch {
            root,
            debounce_ms,
            poll_seconds,
        } => watch(
            root,
            index_path,
            Duration::from_millis(debounce_ms),
            poll_seconds.map(Duration::from_secs),
        )?,
    }

    Ok(())
}

fn default_index_path() -> Result<PathBuf> {
    let base = dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .context("could not resolve a data directory for the index")?;
    Ok(base.join("zapper/index.json"))
}

fn build_index(root: &Path) -> Result<Index> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("could not resolve root {}", root.display()))?;
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("skip walk entry: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() || !is_markdown(entry.path()) {
            continue;
        }

        match index_file(entry.path()) {
            Ok(indexed) => {
                files.insert(entry.path().to_path_buf(), indexed);
            }
            Err(err) => {
                eprintln!("skip {}: {err:#}", entry.path().display());
            }
        }
    }

    Ok(Index {
        version: 1,
        root,
        generated_at: unix_now(),
        files,
    })
}

fn should_descend(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !matches!(
        name.as_ref(),
        ".git"
            | "target"
            | "node_modules"
            | ".cache"
            | ".cargo"
            | ".devdata"
            | ".rustup"
            | ".vim"
            | ".venv"
            | "vendor"
            | "tmp"
    )
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "mdown"
            )
        })
        .unwrap_or(false)
}

fn index_file(path: &Path) -> Result<IndexedFile> {
    let metadata = fs::metadata(path)?;
    let content =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(IndexedFile {
        modified: system_time_to_unix(metadata.modified().unwrap_or(UNIX_EPOCH)),
        size: metadata.len(),
        lines: content.lines().map(ToOwned::to_owned).collect(),
    })
}

fn save_index(path: &Path, index: &Index) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let data = serde_json::to_vec(index)?;
    fs::write(&tmp_path, data)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn load_index(path: &Path) -> Result<Index> {
    let data = fs::read(path).with_context(|| {
        format!(
            "could not read index {}; run `zap index` or start `zap watch` first",
            path.display()
        )
    })?;
    Ok(serde_json::from_slice(&data)?)
}

fn search_index(index: &Index, keyword: &str, limit: usize) -> Vec<Match> {
    let needle = keyword.to_lowercase();
    if needle.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for (path, file) in &index.files {
        for (line_idx, line) in file.lines.iter().enumerate() {
            let lower_line = line.to_lowercase();
            for byte_idx in find_all(&lower_line, &needle) {
                matches.push(Match {
                    path: path.clone(),
                    line_number: line_idx + 1,
                    column: char_column(line, byte_idx),
                    line: line.clone(),
                });
                if matches.len() >= limit {
                    return matches;
                }
            }
        }
    }
    matches
}

fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut offset = 0;
    while let Some(found) = haystack[offset..].find(needle) {
        let position = offset + found;
        positions.push(position);
        offset = position + needle.len();
    }
    positions
}

fn char_column(line: &str, byte_idx: usize) -> usize {
    line[..byte_idx].chars().count() + 1
}

fn watch(
    root: PathBuf,
    index_path: PathBuf,
    debounce: Duration,
    poll_interval: Option<Duration>,
) -> Result<()> {
    if let Some(interval) = poll_interval {
        return watch_by_polling(root, index_path, interval);
    }

    rebuild(&root, &index_path)?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default())?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    eprintln!(
        "watching {} and writing {}",
        root.display(),
        index_path.display()
    );

    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                if !event
                    .paths
                    .iter()
                    .any(|path| path.is_dir() || is_markdown(path))
                {
                    continue;
                }
                std::thread::sleep(debounce);
                drain_pending(&rx);
                if let Err(err) = rebuild(&root, &index_path) {
                    eprintln!("index update failed: {err:#}");
                }
            }
            Ok(Err(err)) => eprintln!("watch error: {err:#}"),
            Err(err) => return Err(err.into()),
        }
    }
}

fn watch_by_polling(root: PathBuf, index_path: PathBuf, interval: Duration) -> Result<()> {
    let mut state = rebuild_with_state(&root, &index_path)?;
    eprintln!(
        "polling {} every {}s and writing {}",
        root.display(),
        interval.as_secs(),
        index_path.display()
    );

    loop {
        std::thread::sleep(interval);
        let next_state = collect_file_state(&root)?;
        if next_state != state {
            match rebuild_with_state(&root, &index_path) {
                Ok(updated) => state = updated,
                Err(err) => eprintln!("index update failed: {err:#}"),
            }
        }
    }
}

fn drain_pending(rx: &mpsc::Receiver<notify::Result<notify::Event>>) {
    while rx.try_recv().is_ok() {}
}

fn rebuild(root: &Path, index_path: &Path) -> Result<()> {
    let index = build_index(root)?;
    let count = index.files.len();
    save_index(index_path, &index)?;
    eprintln!("indexed {count} markdown files");
    Ok(())
}

fn rebuild_with_state(root: &Path, index_path: &Path) -> Result<BTreeMap<PathBuf, FileState>> {
    let index = build_index(root)?;
    let count = index.files.len();
    let state = index
        .files
        .iter()
        .map(|(path, file)| {
            (
                path.clone(),
                FileState {
                    modified: file.modified,
                    size: file.size,
                },
            )
        })
        .collect();
    save_index(index_path, &index)?;
    eprintln!("indexed {count} markdown files");
    Ok(state)
}

fn collect_file_state(root: &Path) -> Result<BTreeMap<PathBuf, FileState>> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("could not resolve root {}", root.display()))?;
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("skip walk entry: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() || !is_markdown(entry.path()) {
            continue;
        }
        match fs::metadata(entry.path()) {
            Ok(metadata) => {
                files.insert(
                    entry.path().to_path_buf(),
                    FileState {
                        modified: system_time_to_unix(metadata.modified().unwrap_or(UNIX_EPOCH)),
                        size: metadata.len(),
                    },
                );
            }
            Err(err) => eprintln!("skip {}: {err:#}", entry.path().display()),
        }
    }

    Ok(files)
}

fn unix_now() -> u64 {
    system_time_to_unix(SystemTime::now())
}

fn system_time_to_unix(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
