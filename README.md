# scip-php

A [SCIP](https://github.com/sourcegraph/scip) indexer for PHP projects, written in Rust.

**scip-php** parses PHP codebases and emits a SCIP index file (`index.scip`) that enables rich code intelligence features — go-to-definition, find-all-references, hover documentation, and more — in tools like [Sourcegraph](https://sourcegraph.com).

## Features

- Indexes PHP classes, interfaces, enums, traits, functions, methods, and properties
- Resolves namespaces and cross-file references using PSR-4 autoloading via `composer.json`
- Extracts PHPDoc documentation for symbols
- Outputs a standard SCIP protobuf file compatible with Sourcegraph and other SCIP consumers
- Fast: implemented in Rust using the [Mago](https://github.com/carthage-software/mago) PHP parser

## Installation

### Pre-built binaries

Download a pre-built binary for your platform from the [releases page](https://github.com/ArtemisMucaj/scip-php/releases).

Supported platforms:
- Linux (x86_64)
- macOS (aarch64)
- Windows (x86_64)

### Build from source

Requires Rust 1.93 or later.

```bash
git clone https://github.com/ArtemisMucaj/scip-php
cd scip-php
cargo build --release
```

The binary will be at `target/release/scip-php`.

## Usage

```
scip-php [PROJECT_ROOT] [-o <OUTPUT>]
```

**Arguments:**

| Argument | Description | Default |
|----------|-------------|---------|
| `PROJECT_ROOT` | Path to the PHP project root (must contain a `composer.json`) | `.` |
| `-o, --output <OUTPUT>` | Output path for the SCIP index file | `index.scip` |

**Example:**

```bash
# Index the current PHP project
scip-php

# Index a specific project and write output to a custom path
scip-php /path/to/my-php-project -o /tmp/my-project.scip
```

After running, the tool reports the number of documents, occurrences, and symbols indexed, along with elapsed time.

## Requirements

- The PHP project must have a `composer.json` with a `autoload.psr-4` section so that scip-php can discover source files and resolve namespaces.
- No PHP runtime is required — scip-php is a self-contained binary.

## How it works

1. **Project discovery** — reads `composer.json` to find PSR-4 autoload paths and enumerate all `.php` files.
2. **Parsing** — each file is parsed into an AST using the Mago PHP parser.
3. **Name resolution** — namespace imports and fully-qualified names are resolved across the project.
4. **Indexing** — the AST is walked to collect symbol definitions and occurrences (references). PHPDoc comments are extracted for hover documentation.
5. **Serialization** — the collected data is written as a binary SCIP protobuf file.

## Development

### Running tests

```bash
cargo test --verbose
```

### Project structure

```
src/
  main.rs         CLI entry point and argument parsing
  lib.rs          Module declarations
  indexer.rs      Core AST walking and SCIP emission logic
  project.rs      PHP project discovery via composer.json
  symbol.rs       SCIP symbol identifier construction
  line_index.rs   Byte offset to line/column mapping
tests/
  integration_test.rs          Integration tests
  fixtures/sample-project/     Sample PHP project used by tests
```

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for release history.

## License

MIT
