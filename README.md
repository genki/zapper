# zapper

`zapper` is a local Markdown search engine. The CLI command is `zap`.

It indexes Markdown files, watches for changes, and returns keyword matches as full paths with line and column positions.

## Commands

Build an index:

```sh
zap index --root /home/vagrant
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

For broad trees such as a whole home directory, polling mode avoids creating a
large recursive inotify watch set:

```sh
zap watch --root /home/vagrant --poll-seconds 10
```

Use a custom index path:

```sh
zap --index /tmp/zapper-index.json index --root .
zap --index /tmp/zapper-index.json search "keyword"
```

## Watcher service

A systemd unit template is provided at `packaging/systemd/zapper.service`.

The service runs:

```sh
/usr/local/bin/zap --index /home/vagrant/.local/share/zapper/index.json watch --root /home/vagrant --poll-seconds 10
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

`zap search keyword` prints tab-separated rows:

```text
/full/path/file.md    12    8    matching line text
```

Columns are:

- full path
- 1-based line number
- 1-based character position within the line
- line text
