# zapper

`zapper` is a local Markdown search engine. The CLI command is `zap`.

It indexes Markdown files, watches for changes, and returns keyword matches as full paths with line and column positions.

## v0.1

Version `0.1.0` provides:

- Markdown indexing for `.md`, `.markdown`, and `.mdown` files
- automatic index refresh when Markdown files are added, updated, or deleted
- default include patterns for common workspace Markdown:
  `~/*.md`, `~/*/*.md`, and `~/*/memo/**/*.md`
- defaults can be changed with `~/.config/zapper/config.json`
- `--all` for indexing or watching every Markdown file under the root
- literal, case-insensitive keyword search
- search results with full path, 1-based line number, 1-based character position, and line text
- `search` as the default command, so `zap keyword` works
- stdin keyword input, so `printf 'keyword' | zap` works
- a systemd watcher service template
- a `zap(1)` manual page

## Commands

Build an index:

```sh
zap index --root /home/vagrant
```

By default, `index` and `watch` focus on common workspace Markdown locations:

```text
~/*.md
~/*/*.md
~/*/memo/**/*.md
```

Use `--all` to include every Markdown file under the root:

```sh
zap index --root /home/vagrant --all
```

Search:

```sh
zap search "keyword"
```

The `search` subcommand is the default, so this is equivalent:

```sh
zap "keyword"
```

`search` also accepts the keyword from stdin when it is piped:

```sh
printf 'keyword' | zap --limit 20
printf 'keyword' | zap search --limit 20
```

Run the watcher in the foreground:

```sh
zap watch --root /home/vagrant
```

The watcher uses filesystem notifications by default. For the default config,
it registers watches only for the root, first-level workspace directories, and
matching `memo` subtrees. On this host that reduced the watch set from 49,126
directories for a full recursive home-directory watch to 163 directories.

Polling is still available as an explicit fallback:

```sh
zap watch --root /home/vagrant --poll-seconds 10
```

Use a custom index path:

```sh
zap --index /tmp/zapper-index.json index --root .
zap --index /tmp/zapper-index.json search "keyword"
```

Use a custom config file:

```sh
zap --config /path/to/config.json index
zap --config /path/to/config.json watch
```

Default config path:

```text
~/.config/zapper/config.json
```

Example config:

```json
{
  "root": "/home/vagrant",
  "index": "/home/vagrant/.local/share/zapper/index.json",
  "all": false,
  "include_patterns": [
    "~/*.md",
    "~/*/*.md",
    "~/*/memo/**/*.md"
  ],
  "watch_patterns": [
    "~",
    "~/*",
    "~/*/memo",
    "~/*/memo/**"
  ]
}
```

`include_patterns` decide which Markdown files are indexed. `watch_patterns`
decide which directories get filesystem notification watches. Relative patterns
are resolved from `root`; `~/` is expanded to the current user's home directory.

## Watcher service

A systemd unit template is provided at `packaging/systemd/zapper.service`.

The service runs:

```sh
/usr/local/bin/zap --index /home/vagrant/.local/share/zapper/index.json watch --root /home/vagrant
```

Install example:

```sh
cargo build --release
sudo install -m 0755 target/release/zap /usr/local/bin/zap
sudo install -m 0644 packaging/systemd/zapper.service /etc/systemd/system/zapper.service
sudo install -D -m 0644 man/zap.1 /usr/local/share/man/man1/zap.1
sudo systemctl daemon-reload
sudo systemctl enable --now zapper.service
```

After installing the manual page:

```sh
man zap
```

## Output

`zap search keyword` and `zap keyword` print tab-separated rows:

```text
/full/path/file.md    12    8    matching line text
```

Columns are:

- full path
- 1-based line number
- 1-based character position within the line
- line text

## Verification

The installed service on this host was checked with temporary Markdown files
under `/home/vagrant`, which is the configured service root.

Measured scope on this host:

```text
default scope: 7,842 Markdown files, 163 watched directories
--all scope:   11,391 Markdown files, 49,126 watched directories
```

```sh
printf 'intro line\nprefix ZAPV01_CREATE_TOKEN suffix\n' > /home/vagrant/zapper-v01-watch-check.md
sleep 18
zap ZAPV01_CREATE_TOKEN --limit 5
```

Result:

```text
/home/vagrant/zapper-v01-watch-check.md    2    8    prefix ZAPV01_CREATE_TOKEN suffix
```

After replacing the file content, the old token disappeared and the new token was
found at the updated line and position:

```text
/home/vagrant/zapper-v01-watch-check.md    3    5    abc ZAPV01_UPDATE_TOKEN suffix
```

After deleting the file, the update token no longer appeared in search results.
The service remained active with no restarts:

```sh
systemctl is-active zapper.service
systemctl show -p NRestarts --value zapper.service
```
