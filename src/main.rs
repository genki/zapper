use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use glob::glob;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
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

        #[arg(long = "remote", value_name = "URL")]
        remote_endpoints: Vec<String>,

        #[arg(long = "remote-token", value_name = "TOKEN")]
        remote_tokens: Vec<String>,

        #[arg(long = "remote-ca-cert", value_name = "PATH")]
        remote_ca_certs: Vec<PathBuf>,

        #[arg(long)]
        no_local: bool,
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
    /// Serve the search API over HTTPS.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: SocketAddr,

        #[arg(long, value_name = "TOKEN", env = "ZAP_API_TOKEN")]
        token: Option<String>,

        #[arg(long, value_name = "HOST")]
        host_label: Option<String>,

        #[arg(long = "tls-cert", value_name = "PATH")]
        tls_cert: Option<PathBuf>,

        #[arg(long = "tls-key", value_name = "PATH")]
        tls_key: Option<PathBuf>,
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
    api_token: Option<String>,
    host_label: Option<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    remotes: Option<Vec<RemoteConfig>>,
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

#[derive(Debug, Clone, Deserialize)]
struct RemoteConfig {
    endpoint: String,
    token: String,
    host: Option<String>,
    ca_cert: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiSearchResponse {
    host: String,
    matches: Vec<ApiMatch>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiMatch {
    path: String,
    line_number: usize,
    column: usize,
    line: String,
}

#[derive(Debug)]
struct DisplayMatch {
    path: String,
    line_number: usize,
    column: usize,
    line: String,
}

#[derive(Debug)]
struct HttpsEndpoint {
    host: String,
    port: u16,
    path: String,
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
        Command::Search {
            keyword,
            limit,
            remote_endpoints,
            remote_tokens,
            remote_ca_certs,
            no_local,
        } => {
            let keyword = resolve_keyword(keyword)?;
            let mut results = Vec::new();
            if !no_local {
                let index = load_index(&index_path)?;
                results.extend(
                    search_index(&index, &keyword, limit)
                        .into_iter()
                        .map(DisplayMatch::from),
                );
            }
            for remote in
                resolve_remotes(&config, remote_endpoints, remote_tokens, remote_ca_certs)?
            {
                match search_remote(&remote, &keyword, limit) {
                    Ok(remote_results) => results.extend(remote_results),
                    Err(err) => eprintln!("remote search failed for {}: {err:#}", remote.endpoint),
                }
            }
            for item in results.into_iter().take(limit) {
                println!(
                    "{}\t{}\t{}\t{}",
                    item.path, item.line_number, item.column, item.line
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
        Command::Serve {
            bind,
            token,
            host_label,
            tls_cert,
            tls_key,
        } => serve_search_api(
            index_path,
            bind,
            token.or(config.api_token).context(
                "API token is required; use --token, ZAP_API_TOKEN, or config api_token",
            )?,
            host_label.or(config.host_label),
            tls_cert
                .or(config.tls_cert)
                .context("TLS certificate is required; use --tls-cert or config tls_cert")?,
            tls_key
                .or(config.tls_key)
                .context("TLS private key is required; use --tls-key or config tls_key")?,
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
    matches!(arg.to_str(), Some("index" | "search" | "watch" | "serve"))
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

impl From<Match> for DisplayMatch {
    fn from(value: Match) -> Self {
        Self {
            path: value.path.display().to_string(),
            line_number: value.line_number,
            column: value.column,
            line: value.line,
        }
    }
}

fn resolve_remotes(
    config: &ZapConfig,
    endpoints: Vec<String>,
    tokens: Vec<String>,
    ca_certs: Vec<PathBuf>,
) -> Result<Vec<RemoteConfig>> {
    let mut remotes = config.remotes.clone().unwrap_or_default();
    if endpoints.is_empty() {
        return Ok(remotes);
    }

    if tokens.len() != endpoints.len() && tokens.len() != 1 {
        anyhow::bail!("--remote-token must be specified once for all remotes or once per --remote");
    }
    if !ca_certs.is_empty() && ca_certs.len() != endpoints.len() && ca_certs.len() != 1 {
        anyhow::bail!(
            "--remote-ca-cert must be specified once for all remotes or once per --remote"
        );
    }

    for (index, endpoint) in endpoints.into_iter().enumerate() {
        let token = if tokens.len() == 1 {
            tokens[0].clone()
        } else {
            tokens[index].clone()
        };
        let ca_cert = if ca_certs.is_empty() {
            None
        } else if ca_certs.len() == 1 {
            Some(ca_certs[0].clone())
        } else {
            Some(ca_certs[index].clone())
        };
        remotes.push(RemoteConfig {
            endpoint,
            token,
            host: None,
            ca_cert,
        });
    }

    Ok(remotes)
}

fn search_remote(remote: &RemoteConfig, keyword: &str, limit: usize) -> Result<Vec<DisplayMatch>> {
    let endpoint = parse_https_endpoint(&remote.endpoint)?;
    let ca_cert = remote
        .ca_cert
        .as_deref()
        .context("remote CA certificate is required for HTTPS; use --remote-ca-cert or config remotes[].ca_cert")?;
    let response = remote_get_search(&endpoint, ca_cert, &remote.token, keyword, limit)?;

    let host = remote
        .host
        .clone()
        .filter(|host| !host.is_empty())
        .or_else(|| {
            if response.host.is_empty() {
                None
            } else {
                Some(response.host)
            }
        })
        .or_else(|| Some(endpoint.host.clone()))
        .unwrap_or_else(|| "remote".to_owned());

    Ok(response
        .matches
        .into_iter()
        .map(|item| DisplayMatch {
            path: format!("{host}:{}", item.path),
            line_number: item.line_number,
            column: item.column,
            line: item.line,
        })
        .collect())
}

fn parse_https_endpoint(endpoint: &str) -> Result<HttpsEndpoint> {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        anyhow::bail!("only https:// zapper endpoints are supported: {endpoint}");
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, "search"));
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        (
            host.to_owned(),
            port.parse().context("invalid endpoint port")?,
        )
    } else {
        (authority.to_owned(), 443)
    };
    if host.is_empty() {
        anyhow::bail!("endpoint host is empty: {endpoint}");
    }
    let path = if path.is_empty() {
        "/search".to_owned()
    } else {
        format!("/{path}")
    };
    Ok(HttpsEndpoint { host, port, path })
}

fn remote_get_search(
    endpoint: &HttpsEndpoint,
    ca_cert: &Path,
    token: &str,
    keyword: &str,
    limit: usize,
) -> Result<ApiSearchResponse> {
    let addr = format!("{}:{}", endpoint.host, endpoint.port);
    let tcp_stream =
        TcpStream::connect(&addr).with_context(|| format!("could not connect to {addr}"))?;
    let server_name = ServerName::try_from(endpoint.host.clone())
        .with_context(|| format!("invalid TLS server name {}", endpoint.host))?;
    let client_config = Arc::new(load_client_tls_config(ca_cert)?);
    let connection = ClientConnection::new(client_config, server_name)
        .context("could not create TLS client connection")?;
    let mut stream = StreamOwned::new(connection, tcp_stream);
    let path = format!(
        "{}?q={}&limit={}",
        endpoint.path,
        percent_encode(keyword),
        limit
    );
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nX-Zap-Token: {token}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        endpoint.host
    )?;

    let mut response = Vec::new();
    match stream.read_to_end(&mut response) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {}
        Err(err) => return Err(err).context("could not read HTTPS response"),
    }
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("invalid HTTP response")?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .context("invalid HTTP status")?;
    if status != 200 {
        anyhow::bail!("remote API returned HTTP {status}");
    }
    serde_json::from_slice(&response[header_end + 4..]).context("invalid JSON response")
}

fn load_client_tls_config(ca_cert: &Path) -> Result<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for cert in load_certificates(ca_cert)? {
        roots
            .add(cert)
            .with_context(|| format!("could not add CA certificate {}", ca_cert.display()))?;
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn load_server_tls_config(cert_path: &Path, key_path: &Path) -> Result<ServerConfig> {
    let certs = load_certificates(cert_path)?;
    if certs.is_empty() {
        anyhow::bail!(
            "TLS certificate file has no certificates: {}",
            cert_path.display()
        );
    }
    let key = load_private_key(key_path)?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .with_context(|| {
            format!(
                "could not configure TLS with cert {} and key {}",
                cert_path.display(),
                key_path.display()
            )
        })
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let data = fs::read(path)
        .with_context(|| format!("could not read certificate file {}", path.display()))?;
    rustls_pemfile::certs(&mut &data[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("could not parse certificate file {}", path.display()))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let data =
        fs::read(path).with_context(|| format!("could not read private key {}", path.display()))?;
    rustls_pemfile::private_key(&mut &data[..])
        .with_context(|| format!("could not parse private key {}", path.display()))?
        .with_context(|| format!("private key file has no key: {}", path.display()))
}

fn serve_search_api(
    index_path: PathBuf,
    bind: SocketAddr,
    token: String,
    host_label: Option<String>,
    tls_cert: PathBuf,
    tls_key: PathBuf,
) -> Result<()> {
    if token.is_empty() {
        anyhow::bail!("API token must not be empty");
    }
    let host_label = host_label.unwrap_or_else(default_host_label);
    let tls_config = Arc::new(load_server_tls_config(&tls_cert, &tls_key)?);
    let listener = TcpListener::bind(bind).with_context(|| format!("could not bind {bind}"))?;
    eprintln!("serving zap search API on https://{bind}/search as {host_label}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let connection = ServerConnection::new(tls_config.clone())
                    .context("could not create TLS server connection");
                let result = connection.and_then(|connection| {
                    let stream = StreamOwned::new(connection, stream);
                    handle_api_connection(stream, &index_path, &token, &host_label)
                });
                if let Err(err) = result {
                    eprintln!("API request failed: {err:#}");
                }
            }
            Err(err) => eprintln!("API accept failed: {err:#}"),
        }
    }

    Ok(())
}

fn handle_api_connection<S>(
    mut stream: S,
    index_path: &Path,
    token: &str,
    host_label: &str,
) -> Result<()>
where
    S: Read + Write,
{
    let mut reader = BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.trim().is_empty() {
        return Ok(());
    }

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        headers.push(trimmed.to_owned());
    }
    drop(reader);

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    if method != "GET" {
        return write_text_response(&mut stream, 405, "method not allowed\n");
    }
    if target == "/health" {
        return write_text_response(&mut stream, 200, "ok\n");
    }
    if !api_token_matches(&headers, token) {
        return write_text_response(&mut stream, 401, "unauthorized\n");
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/search" {
        return write_text_response(&mut stream, 404, "not found\n");
    }

    let mut keyword = None;
    let mut limit = 200usize;
    for (key, value) in parse_query(query) {
        match key.as_str() {
            "q" | "keyword" => keyword = Some(value),
            "limit" => limit = value.parse().unwrap_or(limit),
            _ => {}
        }
    }

    let Some(keyword) = keyword else {
        return write_text_response(&mut stream, 400, "missing q\n");
    };

    let index = load_index(index_path)?;
    let response = ApiSearchResponse {
        host: host_label.to_owned(),
        matches: search_index(&index, &keyword, limit)
            .into_iter()
            .map(|item| ApiMatch {
                path: item.path.display().to_string(),
                line_number: item.line_number,
                column: item.column,
                line: item.line,
            })
            .collect(),
    };
    write_json_response(&mut stream, 200, &response)
}

fn api_token_matches(headers: &[String], token: &str) -> bool {
    headers.iter().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        let name = name.trim();
        let value = value.trim();
        (name.eq_ignore_ascii_case("authorization") && value == format!("Bearer {token}"))
            || (name.eq_ignore_ascii_case("x-zap-token") && value == token)
    })
}

fn write_json_response<S, T>(stream: &mut S, status: u16, value: &T) -> Result<()>
where
    S: Write,
    T: Serialize,
{
    let body = serde_json::to_vec(value)?;
    let reason = reason_phrase(status);
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

fn write_text_response<S>(stream: &mut S, status: u16, body: &str) -> Result<()>
where
    S: Write,
{
    let reason = reason_phrase(status);
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(*byte as char)
            }
            b' ' => output.push('+'),
            byte => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

fn percent_decode(value: &str) -> String {
    let mut bytes = Vec::new();
    let mut iter = value.as_bytes().iter().copied().peekable();
    while let Some(byte) = iter.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hi = iter.next();
                let lo = iter.next();
                match (hi.and_then(hex_value), lo.and_then(hex_value)) {
                    (Some(hi), Some(lo)) => bytes.push((hi << 4) | lo),
                    _ => {
                        bytes.push(b'%');
                        if let Some(hi) = hi {
                            bytes.push(hi);
                        }
                        if let Some(lo) = lo {
                            bytes.push(lo);
                        }
                    }
                }
            }
            byte => bytes.push(byte),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn default_host_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_owned())
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
                                    eprintln!("watching {watched_dirs} directories");
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
