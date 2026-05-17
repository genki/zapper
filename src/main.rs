use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use glob::glob;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

#[derive(Parser, Debug)]
#[command(name = "zap")]
#[command(about = "Local Markdown search engine", version)]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    index: Option<PathBuf>,

    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build or rebuild the Markdown index.
    Index {
        #[arg(long, value_name = "PATH")]
        root: Option<PathBuf>,

        #[arg(long)]
        all: bool,

        #[arg(long = "include", value_name = "GLOB")]
        include_patterns: Vec<String>,
    },
    /// Search indexed Markdown files for a keyword.
    Search {
        keyword: Option<String>,

        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Watch Markdown files and update the index when they change.
    Watch {
        #[arg(long, value_name = "PATH")]
        root: Option<PathBuf>,

        #[arg(long)]
        all: bool,

        #[arg(long = "include", value_name = "GLOB")]
        include_patterns: Vec<String>,

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

#[derive(Debug, Default, Deserialize)]
struct ZapConfig {
    root: Option<PathBuf>,
    index: Option<PathBuf>,
    all: Option<bool>,
    include_patterns: Option<Vec<String>>,
    watch_patterns: Option<Vec<String>>,
}

#[derive(Debug)]
struct Scope {
    root: PathBuf,
    all: bool,
    include: GlobSet,
    watch_patterns: Vec<String>,
}

#[derive(Debug)]
struct Match {
    path: PathBuf,
    line_number: usize,
    column: usize,
    line: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse_from(default_search_args(std::env::args_os()));
    let config = load_config(cli.config.as_deref())?;
    let index_path = match cli.index.or(config.index.clone()) {
        Some(path) => path,
        None => default_index_path()?,
    };

    match cli.command {
        Command::Index {
            root,
            all,
            include_patterns,
        } => {
            let scope = make_scope(root, all, include_patterns, &config)?;
            let index = build_index(&scope)?;
            save_index(&index_path, &index)?;
            eprintln!(
                "indexed {} markdown files into {}",
                index.files.len(),
                index_path.display()
            );
        }
        Command::Search { keyword, limit } => {
            let keyword = resolve_keyword(keyword)?;
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
            all,
            include_patterns,
            debounce_ms,
            poll_seconds,
        } => watch(
            make_scope(root, all, include_patterns, &config)?,
            index_path,
            Duration::from_millis(debounce_ms),
            poll_seconds.map(Duration::from_secs),
        )?,
    }

    Ok(())
}

fn default_search_args<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args: Vec<OsString> = args.into_iter().collect();
    if args.len() <= 1 {
        if !io::stdin().is_terminal() {
            args.push(OsString::from("search"));
        }
        return args;
    }

    if needs_default_search(&args[1..]) {
        args.insert(1, OsString::from("search"));
    }
    args
}

fn needs_default_search(args: &[OsString]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_os_str();
        if is_help_or_version(arg) || is_known_command(arg) {
            return false;
        }
        if arg == OsStr::new("--index") || arg == OsStr::new("--config") {
            index += 2;
            continue;
        }
        if os_str_starts_with(arg, "--index=") || os_str_starts_with(arg, "--config=") {
            index += 1;
            continue;
        }
        if arg == OsStr::new("--") {
            return index + 1 < args.len();
        }
        if os_str_starts_with(arg, "-") {
            index += 1;
            continue;
        }
        return true;
    }
    !io::stdin().is_terminal()
}

fn is_help_or_version(arg: &OsStr) -> bool {
    matches!(
        arg.to_str(),
        Some("-h" | "--help" | "-V" | "--version" | "help")
    )
}

fn is_known_command(arg: &OsStr) -> bool {
    matches!(arg.to_str(), Some("index" | "search" | "watch"))
}

fn os_str_starts_with(value: &OsStr, prefix: &str) -> bool {
    value
        .to_str()
        .map(|value| value.starts_with(prefix))
        .unwrap_or(false)
}

fn resolve_keyword(keyword: Option<String>) -> Result<String> {
    if let Some(keyword) = keyword {
        return Ok(keyword);
    }
    if io::stdin().is_terminal() {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        anyhow::bail!("missing search keyword");
    }

    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("could not read search keyword from stdin")?;
    Ok(input.trim().to_owned())
}

fn default_index_path() -> Result<PathBuf> {
    let base = dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .context("could not resolve a data directory for the index")?;
    Ok(base.join("zapper/index.json"))
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join("zapper/config.json"))
}

fn load_config(path: Option<&Path>) -> Result<ZapConfig> {
    let Some(path) = path.map(Path::to_path_buf).or_else(default_config_path) else {
        return Ok(ZapConfig::default());
    };
    if !path.is_file() {
        return Ok(ZapConfig::default());
    }
    let data =
        fs::read(&path).with_context(|| format!("could not read config {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("could not parse config {}", path.display()))
}

fn make_scope(
    root: Option<PathBuf>,
    all: bool,
    include_patterns: Vec<String>,
    config: &ZapConfig,
) -> Result<Scope> {
    let root = root
        .or_else(|| config.root.clone())
        .unwrap_or(std::env::current_dir().context("could not resolve current directory")?);
    let root = fs::canonicalize(&root)
        .with_context(|| format!("could not resolve root {}", root.display()))?;
    let all = all || config.all.unwrap_or(false);
    let include_patterns = if include_patterns.is_empty() {
        config
            .include_patterns
            .clone()
            .unwrap_or_else(|| default_include_patterns(&root))
    } else {
        include_patterns
    };
    let watch_patterns = config
        .watch_patterns
        .clone()
        .unwrap_or_else(|| default_watch_patterns(&root));
    let include = build_glob_set(&root, &include_patterns)?;

    Ok(Scope {
        root,
        all,
        include,
        watch_patterns,
    })
}

fn build_glob_set(root: &Path, patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(&normalize_pattern(root, pattern))
                .literal_separator(true)
                .build()?,
        );
    }
    Ok(builder.build()?)
}

fn default_include_patterns(root: &Path) -> Vec<String> {
    let bases = ["*.md", "*.markdown", "*.mdown"];
    let prefixes = ["", "*/", "*/memo/**/"];
    prefixes
        .iter()
        .flat_map(|prefix| {
            bases
                .iter()
                .map(move |base| root.join(format!("{prefix}{base}")))
        })
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn default_watch_patterns(root: &Path) -> Vec<String> {
    ["", "*/", "*/memo/", "*/memo/**/"]
        .iter()
        .map(|pattern| root.join(pattern).to_string_lossy().into_owned())
        .collect()
}

fn normalize_pattern(root: &Path, pattern: &str) -> String {
    if let Some(home_pattern) = pattern.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(home_pattern).to_string_lossy().into_owned();
        }
    }
    let path = Path::new(pattern);
    if path.is_absolute() {
        pattern.to_owned()
    } else {
        root.join(path).to_string_lossy().into_owned()
    }
}

fn build_index(scope: &Scope) -> Result<Index> {
    let root = &scope.root;
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
        if !entry.file_type().is_file() || !is_indexed_markdown(scope, entry.path()) {
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
        root: root.clone(),
        generated_at: unix_now(),
        files,
    })
}

fn should_descend(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !is_excluded_name(&name)
}

fn is_excluded_name(name: &str) -> bool {
    matches!(
        name,
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

fn is_indexed_markdown(scope: &Scope, path: &Path) -> bool {
    if !is_markdown(path) {
        return false;
    }
    scope.all || scope.include.is_match(path)
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
    scope: Scope,
    index_path: PathBuf,
    debounce: Duration,
    poll_interval: Option<Duration>,
) -> Result<()> {
    if let Some(interval) = poll_interval {
        return watch_by_polling(scope, index_path, interval);
    }

    rebuild(&scope, &index_path)?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default())?;
    let mut watched_dirs = watch_scope(&scope, &mut watcher)?;
    eprintln!(
        "watching {} directories under {} with {} scope and writing {}",
        watched_dirs,
        scope.root.display(),
        if scope.all {
            "all"
        } else {
            "configured-pattern"
        },
        index_path.display()
    );

    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    if path.is_dir() && should_watch_path(path) {
                        match watch_new_directory(&scope, path, &mut watcher) {
                            Ok(count) => {
                                if count > 0 {
                                    watched_dirs += count;
                                }
                            }
                            Err(err) => {
                                eprintln!("watch add failed for {}: {err:#}", path.display())
                            }
                        }
                    }
                }
                if !event
                    .paths
                    .iter()
                    .any(|path| should_rebuild_for_event_path(&scope, path))
                {
                    continue;
                }
                std::thread::sleep(debounce);
                drain_pending(&rx);
                if let Err(err) = rebuild(&scope, &index_path) {
                    eprintln!("index update failed: {err:#}");
                }
            }
            Ok(Err(err)) => eprintln!("watch error: {err:#}"),
            Err(err) => return Err(err.into()),
        }
    }
}

fn watch_scope(scope: &Scope, watcher: &mut RecommendedWatcher) -> Result<usize> {
    if scope.all {
        return watch_directory_tree(&scope.root, watcher);
    }
    watch_configured_scope(scope, watcher)
}

fn watch_directory_tree(root: &Path, watcher: &mut RecommendedWatcher) -> Result<usize> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("could not resolve watch root {}", root.display()))?;
    let mut count = 0;

    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("skip watch entry: {err}");
                continue;
            }
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        match watcher.watch(entry.path(), RecursiveMode::NonRecursive) {
            Ok(()) => count += 1,
            Err(err) => eprintln!("skip watch {}: {err:#}", entry.path().display()),
        }
    }

    Ok(count)
}

fn watch_configured_scope(scope: &Scope, watcher: &mut RecommendedWatcher) -> Result<usize> {
    let mut watched = BTreeMap::new();
    for pattern in &scope.watch_patterns {
        for path in glob(&normalize_pattern(&scope.root, pattern))? {
            let path = match path {
                Ok(path) => path,
                Err(err) => {
                    eprintln!("skip watch glob entry: {err}");
                    continue;
                }
            };
            if !path.is_dir() || !should_watch_path(&path) {
                continue;
            }
            watched.insert(fs::canonicalize(&path).unwrap_or(path), ());
        }
    }

    let mut count = 0;
    for path in watched.keys() {
        count += watch_one_directory(path, watcher);
    }
    Ok(count)
}

fn watch_one_directory(path: &Path, watcher: &mut RecommendedWatcher) -> usize {
    match watcher.watch(path, RecursiveMode::NonRecursive) {
        Ok(()) => 1,
        Err(err) => {
            eprintln!("skip watch {}: {err:#}", path.display());
            0
        }
    }
}

fn watch_new_directory(
    scope: &Scope,
    path: &Path,
    watcher: &mut RecommendedWatcher,
) -> Result<usize> {
    if scope.all {
        return watch_directory_tree(path, watcher);
    }
    if !is_configured_watch_directory(scope, path) {
        return Ok(0);
    }
    Ok(watch_one_directory(path, watcher))
}

fn should_watch_path(path: &Path) -> bool {
    path.components().all(|component| {
        let name = component.as_os_str().to_string_lossy();
        !is_excluded_name(&name)
    })
}

fn is_configured_watch_directory(scope: &Scope, path: &Path) -> bool {
    scope.watch_patterns.iter().any(|pattern| {
        GlobBuilder::new(&normalize_pattern(&scope.root, pattern))
            .literal_separator(true)
            .build()
            .map(|glob| glob.compile_matcher().is_match(path))
            .unwrap_or(false)
    })
}

fn should_rebuild_for_event_path(scope: &Scope, path: &Path) -> bool {
    if path.is_dir() {
        return scope.all || is_configured_watch_directory(scope, path);
    }
    is_indexed_markdown(scope, path)
}

fn watch_by_polling(scope: Scope, index_path: PathBuf, interval: Duration) -> Result<()> {
    let mut state = rebuild_with_state(&scope, &index_path)?;
    eprintln!(
        "polling {} every {}s and writing {}",
        scope.root.display(),
        interval.as_secs(),
        index_path.display()
    );

    loop {
        std::thread::sleep(interval);
        let next_state = collect_file_state(&scope)?;
        if next_state != state {
            match rebuild_with_state(&scope, &index_path) {
                Ok(updated) => state = updated,
                Err(err) => eprintln!("index update failed: {err:#}"),
            }
        }
    }
}

fn drain_pending(rx: &mpsc::Receiver<notify::Result<notify::Event>>) {
    while rx.try_recv().is_ok() {}
}

fn rebuild(scope: &Scope, index_path: &Path) -> Result<()> {
    let index = build_index(scope)?;
    let count = index.files.len();
    save_index(index_path, &index)?;
    eprintln!("indexed {count} markdown files");
    Ok(())
}

fn rebuild_with_state(scope: &Scope, index_path: &Path) -> Result<BTreeMap<PathBuf, FileState>> {
    let index = build_index(scope)?;
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

fn collect_file_state(scope: &Scope) -> Result<BTreeMap<PathBuf, FileState>> {
    let root = &scope.root;
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
        if !entry.file_type().is_file() || !is_indexed_markdown(scope, entry.path()) {
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
