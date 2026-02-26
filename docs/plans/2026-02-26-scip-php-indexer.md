# SCIP PHP Indexer Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a Rust CLI tool that parses PHP projects and emits SCIP index files for code intelligence (navigation, cross-references, hover docs).

**Architecture:** A standalone Rust binary that uses Mago's crates (parser, name resolver, codex, composer, docblock) as libraries to parse PHP source code, resolve names, scan symbols, and emit SCIP protobuf output. The pipeline is: discover files via composer.json → parse all PHP files → resolve names → scan symbols into CodebaseMetadata → populate inheritance → walk AST to emit SCIP occurrences → serialize to `index.scip`.

**Tech Stack:** Rust 1.93+, mago-syntax (parser/AST), mago-names (name resolution), mago-codex (symbols/references), mago-composer (package metadata), mago-docblock (PHPDoc), mago-span (positions), scip crate (protobuf types + serialization), clap (CLI), bumpalo (arena allocation).

---

## Overview

### SCIP Symbol Naming Scheme for PHP

Every SCIP symbol is a string that uniquely identifies a code entity. For PHP:

```
scheme   = "scip-php"
manager  = "composer"
package  = "<vendor/package>" (from composer.json "name" field)
version  = "<version>" (from composer.json "version" or "dev")

Namespace:  scip-php composer vendor/pkg version App/Models/
Class:      scip-php composer vendor/pkg version App/Models/User#
Method:     scip-php composer vendor/pkg version App/Models/User#getName().
Property:   scip-php composer vendor/pkg version App/Models/User#name.
Constant:   scip-php composer vendor/pkg version App/Models/User#MAX_AGE.
Function:   scip-php composer vendor/pkg version App/Utils/helpers/formatDate().
Interface:  scip-php composer vendor/pkg version App/Contracts/UserRepository#
Trait:      scip-php composer vendor/pkg version App/Traits/HasTimestamps#
Enum:       scip-php composer vendor/pkg version App/Enums/Status#
Enum case:  scip-php composer vendor/pkg version App/Enums/Status#Active.
Parameter:  scip-php composer vendor/pkg version App/Models/User#getName().(name)
Type param: scip-php composer vendor/pkg version App/Collections/Collection#[T]
Local var:  local 0, local 1, local 2, ...  (file-scoped counter)
```

### Position Encoding

SCIP uses line/column positions (0-based). Mago uses byte offsets (`Position { offset: u32 }`).
We must convert by scanning source text for newline characters to build a line index.

Range encoding in SCIP:
- Single-line: `[startLine, startChar, endChar]` (3 elements)
- Multi-line: `[startLine, startChar, endLine, endChar]` (4 elements)

Since Rust processes UTF-8 natively, we use `PositionEncoding::UTF8CodeUnitOffsetFromLineStart`.

### Pipeline

```
1. CLI args parsing (project root, output path)
2. Find & read composer.json → package name + version + PSR-4 autoload map
3. Discover PHP files (via autoload paths or directory walk)
4. For each file:
   a. Parse → Program AST
   b. Resolve names → ResolvedNames
   c. Scan → CodebaseMetadata (accumulate across files)
5. Populate codebase (resolve inheritance)
6. For each file:
   a. Walk AST
   b. For each identifier/name node:
      - Determine if definition or reference
      - Compute SCIP symbol string
      - Compute source range (line/col from byte offset)
      - Emit Occurrence
   c. For each definition:
      - Extract documentation (docblock)
      - Compute relationships (extends/implements)
      - Emit SymbolInformation
   d. Package into Document
7. Create Index with Metadata + Documents
8. Serialize to index.scip
```

---

## Task 1: Project Scaffolding

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `.gitignore`

**Step 1: Create the Cargo.toml**

```toml
[package]
name = "scip-php"
version = "0.1.0"
edition = "2024"
description = "SCIP indexer for PHP"
license = "MIT"
rust-version = "1.93"

[dependencies]
scip = "0.6.1"
mago-syntax = "1.13"
mago-names = "1.13"
mago-codex = "1.13"
mago-composer = "1.13"
mago-docblock = "1.13"
mago-span = "1.13"
mago-database = "1.13"
mago-php-version = "1.13"
bumpalo = "3"
clap = { version = "4", features = ["derive"] }
walkdir = "2"
anyhow = "1"
protobuf = "3.7"

[profile.release]
opt-level = 3
lto = true
```

**Step 2: Create .gitignore**

```
/target
*.scip
```

**Step 3: Create a minimal src/main.rs**

```rust
use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "scip-php", about = "SCIP indexer for PHP")]
struct Args {
    /// Path to the PHP project root (containing composer.json)
    #[arg(default_value = ".")]
    project_root: String,

    /// Output file path
    #[arg(short, long, default_value = "index.scip")]
    output: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    println!("scip-php: indexing {} -> {}", args.project_root, args.output);
    Ok(())
}
```

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: Successful build, downloads dependencies.

**Step 5: Verify CLI works**

Run: `cargo run -- --help`
Expected: Shows help text with `project_root` and `--output` arguments.

**Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs .gitignore
git commit -m "feat: scaffold scip-php Rust project with CLI"
```

---

## Task 2: Line Index (Byte Offset → Line/Column Conversion)

SCIP needs line/column positions but Mago provides byte offsets. This is a foundational utility.

**Files:**
- Create: `src/line_index.rs`
- Create: `src/lib.rs` (module declarations)
- Modify: `src/main.rs` (add mod declaration)

**Step 1: Create src/lib.rs with module declaration**

```rust
pub mod line_index;
```

**Step 2: Create src/line_index.rs with tests**

```rust
/// Maps byte offsets to (line, column) positions for SCIP range encoding.
/// All values are 0-based.
pub struct LineIndex {
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
}

impl LineIndex {
    /// Build a line index from source text.
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (offset, byte) in source.as_bytes().iter().enumerate() {
            if *byte == b'\n' {
                line_starts.push((offset + 1) as u32);
            }
        }
        LineIndex { line_starts }
    }

    /// Convert a byte offset to (line, column), both 0-based.
    /// Column is in UTF-8 byte offset from line start.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        let col = offset - self.line_starts[line];
        (line as u32, col)
    }

    /// Encode a byte range as a SCIP range (3 or 4 element i32 vec).
    /// start and end are byte offsets.
    pub fn scip_range(&self, start: u32, end: u32) -> Vec<i32> {
        let (start_line, start_col) = self.line_col(start);
        let (end_line, end_col) = self.line_col(end);
        if start_line == end_line {
            vec![start_line as i32, start_col as i32, end_col as i32]
        } else {
            vec![
                start_line as i32,
                start_col as i32,
                end_line as i32,
                end_col as i32,
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_line() {
        let src = "hello world";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(0), (0, 0));
        assert_eq!(idx.line_col(5), (0, 5));
        assert_eq!(idx.line_col(10), (0, 10));
    }

    #[test]
    fn test_multi_line() {
        let src = "line1\nline2\nline3";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(0), (0, 0)); // 'l' of line1
        assert_eq!(idx.line_col(5), (0, 5)); // '\n'
        assert_eq!(idx.line_col(6), (1, 0)); // 'l' of line2
        assert_eq!(idx.line_col(11), (1, 5)); // '\n'
        assert_eq!(idx.line_col(12), (2, 0)); // 'l' of line3
    }

    #[test]
    fn test_scip_range_single_line() {
        let src = "<?php\nfunction foo() {}";
        let idx = LineIndex::new(src);
        // "foo" starts at offset 15, ends at 18 (on line 1)
        let range = idx.scip_range(15, 18);
        assert_eq!(range, vec![1, 9, 12]); // line 1, col 9..12
    }

    #[test]
    fn test_scip_range_multi_line() {
        let src = "line1\nline2\nline3";
        let idx = LineIndex::new(src);
        // span from offset 0 (line 0, col 0) to offset 16 (line 2, col 4)
        let range = idx.scip_range(0, 16);
        assert_eq!(range, vec![0, 0, 2, 4]);
    }

    #[test]
    fn test_empty_source() {
        let src = "";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(0), (0, 0));
    }
}
```

**Step 3: Update src/main.rs to reference lib**

Add at the top of `src/main.rs`:
```rust
// The library crate is referenced via `scip_php::` paths
```

No explicit `mod` needed in `main.rs` since `lib.rs` is the library root.

**Step 4: Run tests**

Run: `cargo test`
Expected: All 5 tests pass.

**Step 5: Commit**

```bash
git add src/lib.rs src/line_index.rs
git commit -m "feat: add LineIndex for byte offset to line/column conversion"
```

---

## Task 3: SCIP Symbol Builder

Build the PHP-specific SCIP symbol string construction logic.

**Files:**
- Create: `src/symbol.rs`
- Modify: `src/lib.rs` (add module)

**Step 1: Add module to src/lib.rs**

```rust
pub mod line_index;
pub mod symbol;
```

**Step 2: Create src/symbol.rs**

```rust
use scip::types::{Descriptor, Package, Symbol};
use scip::types::descriptor::Suffix;

const SCHEME: &str = "scip-php";
const MANAGER: &str = "composer";

/// Package context for symbol construction.
#[derive(Debug, Clone)]
pub struct PhpPackage {
    pub name: String,
    pub version: String,
}

impl PhpPackage {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        PhpPackage {
            name: name.into(),
            version: version.into(),
        }
    }

    /// Create a placeholder package for files not belonging to any composer package.
    pub fn local() -> Self {
        PhpPackage {
            name: ".".to_string(),
            version: ".".to_string(),
        }
    }

    fn to_scip_package(&self) -> Package {
        Package {
            manager: MANAGER.to_string(),
            name: self.name.clone(),
            version: self.version.clone(),
            ..Default::default()
        }
    }
}

/// Builds SCIP symbol strings for PHP entities.
pub struct SymbolBuilder<'a> {
    package: &'a PhpPackage,
}

impl<'a> SymbolBuilder<'a> {
    pub fn new(package: &'a PhpPackage) -> Self {
        SymbolBuilder { package }
    }

    /// Create a SCIP Symbol from descriptors.
    fn make_symbol(&self, descriptors: Vec<Descriptor>) -> Symbol {
        Symbol {
            scheme: SCHEME.to_string(),
            package: protobuf::MessageField::some(self.package.to_scip_package()),
            descriptors,
            ..Default::default()
        }
    }

    /// Build descriptor for a namespace segment (suffix `/`).
    fn namespace_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Namespace.into(),
            ..Default::default()
        }
    }

    /// Build descriptor for a type (class/interface/trait/enum) (suffix `#`).
    fn type_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Type.into(),
            ..Default::default()
        }
    }

    /// Build descriptor for a term (property/constant/enum case) (suffix `.`).
    fn term_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Term.into(),
            ..Default::default()
        }
    }

    /// Build descriptor for a method/function (suffix `()`).
    fn method_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Method.into(),
            ..Default::default()
        }
    }

    /// Build descriptor for a parameter (suffix `()`).
    fn parameter_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Parameter.into(),
            ..Default::default()
        }
    }

    /// Build descriptor for a type parameter (suffix `[]`).
    fn type_parameter_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::TypeParameter.into(),
            ..Default::default()
        }
    }

    /// Split a fully-qualified PHP name like "App\Models\User" into
    /// namespace parts and a final name.
    fn split_fqn(fqn: &str) -> (Vec<&str>, &str) {
        let fqn = fqn.strip_prefix('\\').unwrap_or(fqn);
        let parts: Vec<&str> = fqn.split('\\').collect();
        if parts.len() <= 1 {
            (vec![], parts.first().copied().unwrap_or(""))
        } else {
            let (ns, name) = parts.split_at(parts.len() - 1);
            (ns.to_vec(), name[0])
        }
    }

    /// Build descriptors from a fully-qualified name's namespace parts.
    fn namespace_descriptors(ns_parts: &[&str]) -> Vec<Descriptor> {
        ns_parts
            .iter()
            .map(|part| Self::namespace_descriptor(part))
            .collect()
    }

    /// Create a symbol for a namespace (e.g., "App\Models").
    pub fn namespace_symbol(&self, fqn: &str) -> Symbol {
        let fqn = fqn.strip_prefix('\\').unwrap_or(fqn);
        let parts: Vec<&str> = fqn.split('\\').collect();
        let descriptors = parts
            .iter()
            .map(|part| Self::namespace_descriptor(part))
            .collect();
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a class-like (class, interface, trait, enum).
    /// `fqn` is e.g. "App\Models\User".
    pub fn class_like_symbol(&self, fqn: &str) -> Symbol {
        let (ns_parts, name) = Self::split_fqn(fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a function.
    /// `fqn` is e.g. "App\Utils\formatDate".
    pub fn function_symbol(&self, fqn: &str) -> Symbol {
        let (ns_parts, name) = Self::split_fqn(fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::method_descriptor(name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a method on a class-like.
    /// `class_fqn` is e.g. "App\Models\User", `method` is e.g. "getName".
    pub fn method_symbol(&self, class_fqn: &str, method: &str) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::method_descriptor(method));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a property on a class-like.
    /// `property` should not include the `$` prefix.
    pub fn property_symbol(&self, class_fqn: &str, property: &str) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::term_descriptor(property));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a class constant.
    pub fn class_constant_symbol(&self, class_fqn: &str, constant: &str) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::term_descriptor(constant));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for an enum case.
    pub fn enum_case_symbol(&self, enum_fqn: &str, case_name: &str) -> Symbol {
        let (ns_parts, enum_name) = Self::split_fqn(enum_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(enum_name));
        descriptors.push(Self::term_descriptor(case_name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a top-level constant.
    /// `fqn` is e.g. "App\Config\VERSION".
    pub fn constant_symbol(&self, fqn: &str) -> Symbol {
        let (ns_parts, name) = Self::split_fqn(fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::term_descriptor(name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a method parameter.
    pub fn parameter_symbol(
        &self,
        class_fqn: &str,
        method: &str,
        param: &str,
    ) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::method_descriptor(method));
        descriptors.push(Self::parameter_descriptor(param));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a function parameter.
    pub fn function_parameter_symbol(
        &self,
        func_fqn: &str,
        param: &str,
    ) -> Symbol {
        let (ns_parts, func_name) = Self::split_fqn(func_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::method_descriptor(func_name));
        descriptors.push(Self::parameter_descriptor(param));
        self.make_symbol(descriptors)
    }

    /// Create a local symbol (file-scoped, for local variables).
    pub fn local_symbol(id: usize) -> String {
        format!("local {}", id)
    }
}

/// Format a SCIP Symbol struct into its string representation.
pub fn format_symbol(symbol: &Symbol) -> String {
    scip::symbol::format_symbol(symbol.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_package() -> PhpPackage {
        PhpPackage::new("vendor/myapp", "1.0.0")
    }

    #[test]
    fn test_class_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.class_like_symbol("App\\Models\\User");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 App/Models/User#");
    }

    #[test]
    fn test_method_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.method_symbol("App\\Models\\User", "getName");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Models/User#getName()."
        );
    }

    #[test]
    fn test_property_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.property_symbol("App\\Models\\User", "name");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Models/User#name."
        );
    }

    #[test]
    fn test_function_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.function_symbol("App\\Utils\\formatDate");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Utils/formatDate()."
        );
    }

    #[test]
    fn test_global_function_no_namespace() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.function_symbol("array_map");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 array_map().");
    }

    #[test]
    fn test_constant_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.constant_symbol("App\\Config\\VERSION");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Config/VERSION."
        );
    }

    #[test]
    fn test_enum_case_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.enum_case_symbol("App\\Enums\\Status", "Active");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Enums/Status#Active."
        );
    }

    #[test]
    fn test_parameter_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.parameter_symbol("App\\Models\\User", "setName", "name");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Models/User#setName().(name)"
        );
    }

    #[test]
    fn test_local_symbol() {
        assert_eq!(SymbolBuilder::local_symbol(0), "local 0");
        assert_eq!(SymbolBuilder::local_symbol(42), "local 42");
    }

    #[test]
    fn test_leading_backslash_stripped() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.class_like_symbol("\\App\\Models\\User");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 App/Models/User#");
    }

    #[test]
    fn test_namespace_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.namespace_symbol("App\\Models");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 App/Models/");
    }
}
```

**Step 3: Run tests**

Run: `cargo test`
Expected: All symbol tests pass.

**Step 4: Commit**

```bash
git add src/symbol.rs src/lib.rs
git commit -m "feat: add SCIP symbol builder for PHP entities"
```

---

## Task 4: Composer Package Discovery

Read composer.json to determine package name, version, and autoload paths.

**Files:**
- Create: `src/project.rs`
- Modify: `src/lib.rs`

**Step 1: Add module to src/lib.rs**

```rust
pub mod line_index;
pub mod project;
pub mod symbol;
```

**Step 2: Create src/project.rs**

```rust
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::symbol::PhpPackage;

/// Represents a PHP project to be indexed.
pub struct PhpProject {
    /// Absolute path to the project root.
    pub root: PathBuf,
    /// Package metadata from composer.json.
    pub package: PhpPackage,
    /// Directories to scan for PHP files (from PSR-4 autoload).
    pub source_dirs: Vec<PathBuf>,
}

impl PhpProject {
    /// Discover a PHP project from the given root directory.
    /// Reads composer.json if present, falls back to sensible defaults.
    pub fn discover(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize project root: {}", root.display()))?;

        let composer_path = root.join("composer.json");
        if composer_path.exists() {
            Self::from_composer(&root, &composer_path)
        } else {
            Ok(Self {
                root: root.clone(),
                package: PhpPackage::local(),
                source_dirs: vec![root],
            })
        }
    }

    fn from_composer(root: &Path, composer_path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(composer_path)
            .with_context(|| format!("Failed to read {}", composer_path.display()))?;

        let composer: mago_composer::ComposerPackage = content
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse composer.json: {:?}", e))?;

        let name = composer
            .name
            .as_deref()
            .unwrap_or(".")
            .to_string();

        let version = composer
            .version
            .as_deref()
            .unwrap_or("dev")
            .to_string();

        // Collect source directories from PSR-4 autoload
        let mut source_dirs = Vec::new();

        if let Some(autoload) = &composer.autoload {
            for (_namespace, paths) in &autoload.psr_4 {
                match paths {
                    mago_composer::schema::AutoloadPsr4Value::Single(p) => {
                        let dir = root.join(p);
                        if dir.exists() {
                            source_dirs.push(dir);
                        }
                    }
                    mago_composer::schema::AutoloadPsr4Value::Multiple(ps) => {
                        for p in ps {
                            let dir = root.join(p);
                            if dir.exists() {
                                source_dirs.push(dir);
                            }
                        }
                    }
                }
            }
        }

        // Also check autoload-dev for test files
        if let Some(autoload_dev) = &composer.autoload_dev {
            for (_namespace, paths) in &autoload_dev.psr_4 {
                match paths {
                    mago_composer::schema::AutoloadPsr4Value::Single(p) => {
                        let dir = root.join(p);
                        if dir.exists() {
                            source_dirs.push(dir);
                        }
                    }
                    mago_composer::schema::AutoloadPsr4Value::Multiple(ps) => {
                        for p in ps {
                            let dir = root.join(p);
                            if dir.exists() {
                                source_dirs.push(dir);
                            }
                        }
                    }
                }
            }
        }

        // If no PSR-4 paths found, scan the whole project
        if source_dirs.is_empty() {
            source_dirs.push(root.to_path_buf());
        }

        Ok(PhpProject {
            root: root.to_path_buf(),
            package: PhpPackage::new(name, version),
            source_dirs,
        })
    }

    /// Discover all PHP files in the project's source directories.
    pub fn discover_php_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for dir in &self.source_dirs {
            for entry in WalkDir::new(dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext == "php" {
                            files.push(path.to_path_buf());
                        }
                    }
                }
            }
        }
        files.sort();
        files
    }

    /// Get the relative path of a file from the project root.
    pub fn relative_path(&self, file_path: &Path) -> String {
        file_path
            .strip_prefix(&self.root)
            .unwrap_or(file_path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: Successful compilation.

**Step 4: Commit**

```bash
git add src/project.rs src/lib.rs
git commit -m "feat: add project discovery with composer.json parsing"
```

---

## Task 5: Core Indexer — Parse, Resolve, and Emit SCIP

This is the main indexer implementation. It ties together Mago's parsing pipeline with SCIP emission.

**Files:**
- Create: `src/indexer.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`

**Step 1: Add module to src/lib.rs**

```rust
pub mod indexer;
pub mod line_index;
pub mod project;
pub mod symbol;
```

**Step 2: Create src/indexer.rs**

This is the largest file. It walks the AST of each file and emits SCIP occurrences.

```rust
use std::path::Path;

use anyhow::{Context, Result};
use mago_span::HasSpan;
use protobuf::MessageField;
use scip::types::{
    self, Document, Index, Metadata, Occurrence, SymbolInformation, ToolInfo,
    PositionEncoding,
};

use crate::line_index::LineIndex;
use crate::project::PhpProject;
use crate::symbol::{format_symbol, PhpPackage, SymbolBuilder};

/// SCIP indexer for PHP projects.
pub struct Indexer {
    project: PhpProject,
}

impl Indexer {
    pub fn new(project: PhpProject) -> Self {
        Indexer { project }
    }

    /// Run the indexer and produce a SCIP Index.
    pub fn index(&self) -> Result<Index> {
        let php_files = self.project.discover_php_files();
        eprintln!("scip-php: found {} PHP files", php_files.len());

        let mut documents = Vec::new();

        for file_path in &php_files {
            match self.index_file(file_path) {
                Ok(Some(doc)) => documents.push(doc),
                Ok(None) => {} // Skip files with no indexable content
                Err(e) => {
                    eprintln!(
                        "scip-php: warning: failed to index {}: {}",
                        file_path.display(),
                        e
                    );
                }
            }
        }

        eprintln!("scip-php: indexed {} documents", documents.len());

        let project_root = format!(
            "file://{}",
            self.project.root.to_string_lossy()
        );

        let index = Index {
            metadata: MessageField::some(Metadata {
                version: types::ProtocolVersion::UnspecifiedProtocolVersion.into(),
                tool_info: MessageField::some(ToolInfo {
                    name: "scip-php".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    ..Default::default()
                }),
                project_root,
                text_document_encoding: types::TextEncoding::UTF8.into(),
                ..Default::default()
            }),
            documents,
            ..Default::default()
        };

        Ok(index)
    }

    /// Index a single PHP file and produce a SCIP Document.
    fn index_file(&self, file_path: &Path) -> Result<Option<Document>> {
        let source = std::fs::read_to_string(file_path)
            .with_context(|| format!("Failed to read {}", file_path.display()))?;

        let relative_path = self.project.relative_path(file_path);
        let line_index = LineIndex::new(&source);

        // Parse the file using Mago
        let arena = bumpalo::Bump::new();
        let file = mago_database::file::File::new(
            mago_database::file::FileId::dummy(),
            relative_path.clone(),
            source.clone(),
        );
        let program = mago_syntax::parse_file(&arena, &file);

        // Check for parse errors — still index what we can
        if !program.errors.is_empty() {
            eprintln!(
                "scip-php: {} parse error(s) in {}",
                program.errors.len(),
                relative_path
            );
        }

        // Resolve names (handles use statements, namespace resolution)
        let resolver = mago_names::NameResolver::new(&arena);
        let resolved_names = resolver.resolve(program);

        // Build SCIP data by walking the AST
        let builder = SymbolBuilder::new(&self.project.package);
        let mut occurrences = Vec::new();
        let mut symbols = Vec::new();
        let mut local_counter: usize = 0;

        // Walk statements to find definitions
        self.walk_statements(
            program.statements,
            &resolved_names,
            &builder,
            &line_index,
            &source,
            &mut occurrences,
            &mut symbols,
            &mut local_counter,
        );

        if occurrences.is_empty() && symbols.is_empty() {
            return Ok(None);
        }

        Ok(Some(Document {
            relative_path,
            language: "PHP".to_string(),
            occurrences,
            symbols,
            position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
            ..Default::default()
        }))
    }

    /// Walk a slice of statements and emit occurrences.
    fn walk_statements(
        &self,
        statements: &[mago_syntax::ast::Statement<'_>],
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        use mago_syntax::ast::Statement;

        for stmt in statements {
            match stmt {
                Statement::Namespace(ns) => {
                    self.walk_namespace(
                        ns, resolved_names, builder, line_index, source,
                        occurrences, symbols, local_counter,
                    );
                }
                Statement::Function(func) => {
                    self.walk_function(
                        func, None, resolved_names, builder, line_index, source,
                        occurrences, symbols, local_counter,
                    );
                }
                Statement::Class(class) => {
                    self.walk_class(
                        class, resolved_names, builder, line_index, source,
                        occurrences, symbols, local_counter,
                    );
                }
                Statement::Interface(iface) => {
                    self.walk_interface(
                        iface, resolved_names, builder, line_index, source,
                        occurrences, symbols, local_counter,
                    );
                }
                Statement::Trait(trait_def) => {
                    self.walk_trait(
                        trait_def, resolved_names, builder, line_index, source,
                        occurrences, symbols, local_counter,
                    );
                }
                Statement::Enum(enum_def) => {
                    self.walk_enum(
                        enum_def, resolved_names, builder, line_index, source,
                        occurrences, symbols, local_counter,
                    );
                }
                Statement::Constant(constant) => {
                    self.walk_constant(
                        constant, resolved_names, builder, line_index, source,
                        occurrences, symbols,
                    );
                }
                _ => {
                    // TODO: Handle expression statements, control flow, etc.
                    // These may contain references to classes/functions that
                    // should be tracked.
                }
            }
        }
    }

    /// Walk a namespace and its body.
    fn walk_namespace(
        &self,
        ns: &mago_syntax::ast::Namespace<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        use mago_syntax::ast::NamespaceBody;

        // Emit namespace definition occurrence
        let ns_name = self.identifier_name(&ns.name, source);
        let ns_symbol = builder.namespace_symbol(&ns_name);
        let ns_symbol_str = format_symbol(&ns_symbol);

        let span = ns.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(span.start.offset, span.end.offset),
            symbol: ns_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        symbols.push(SymbolInformation {
            symbol: ns_symbol_str,
            kind: types::symbol_information::Kind::Namespace.into(),
            display_name: ns_name.clone(),
            ..Default::default()
        });

        // Walk namespace body
        match &ns.body {
            NamespaceBody::Implicit(body) => {
                self.walk_statements(
                    &body.statements, resolved_names, builder, line_index,
                    source, occurrences, symbols, local_counter,
                );
            }
            NamespaceBody::Explicit(body) => {
                self.walk_statements(
                    &body.statements, resolved_names, builder, line_index,
                    source, occurrences, symbols, local_counter,
                );
            }
        }
    }

    /// Walk a class definition and its members.
    fn walk_class(
        &self,
        class: &mago_syntax::ast::Class<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let class_name = self.identifier_name(&class.name, source);
        let fqn = self.resolve_name_at(&class.name, resolved_names, source);

        let class_symbol = builder.class_like_symbol(&fqn);
        let class_symbol_str = format_symbol(&class_symbol);

        // Definition occurrence for the class name
        let name_span = class.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: class_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        // Relationships (extends, implements)
        let mut relationships = Vec::new();
        if let Some(extends) = &class.extends {
            for parent in &extends.types.inner {
                let parent_name = self.resolve_name_at(parent, resolved_names, source);
                let parent_sym = builder.class_like_symbol(&parent_name);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&parent_sym),
                    is_implementation: true,
                    ..Default::default()
                });

                // Reference occurrence for the parent class name
                let parent_span = parent.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(
                        parent_span.start.offset,
                        parent_span.end.offset,
                    ),
                    symbol: format_symbol(&parent_sym),
                    ..Default::default()
                });
            }
        }
        if let Some(implements) = &class.implements {
            for iface in &implements.types.inner {
                let iface_name = self.resolve_name_at(iface, resolved_names, source);
                let iface_sym = builder.class_like_symbol(&iface_name);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&iface_sym),
                    is_implementation: true,
                    ..Default::default()
                });

                let iface_span = iface.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(
                        iface_span.start.offset,
                        iface_span.end.offset,
                    ),
                    symbol: format_symbol(&iface_sym),
                    ..Default::default()
                });
            }
        }

        symbols.push(SymbolInformation {
            symbol: class_symbol_str.clone(),
            kind: types::symbol_information::Kind::Class.into(),
            display_name: class_name.clone(),
            relationships,
            ..Default::default()
        });

        // Walk class members
        for member in &class.body.members {
            self.walk_class_member(
                member, &fqn, resolved_names, builder, line_index, source,
                occurrences, symbols, local_counter,
            );
        }
    }

    /// Walk an interface definition.
    fn walk_interface(
        &self,
        iface: &mago_syntax::ast::Interface<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let iface_name = self.identifier_name(&iface.name, source);
        let fqn = self.resolve_name_at(&iface.name, resolved_names, source);

        let iface_symbol = builder.class_like_symbol(&fqn);
        let iface_symbol_str = format_symbol(&iface_symbol);

        let name_span = iface.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: iface_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let mut relationships = Vec::new();
        if let Some(extends) = &iface.extends {
            for parent in &extends.types.inner {
                let parent_name = self.resolve_name_at(parent, resolved_names, source);
                let parent_sym = builder.class_like_symbol(&parent_name);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&parent_sym),
                    is_implementation: true,
                    ..Default::default()
                });
                let parent_span = parent.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(
                        parent_span.start.offset,
                        parent_span.end.offset,
                    ),
                    symbol: format_symbol(&parent_sym),
                    ..Default::default()
                });
            }
        }

        symbols.push(SymbolInformation {
            symbol: iface_symbol_str.clone(),
            kind: types::symbol_information::Kind::Interface.into(),
            display_name: iface_name,
            relationships,
            ..Default::default()
        });

        for member in &iface.body.members {
            self.walk_class_member(
                member, &fqn, resolved_names, builder, line_index, source,
                occurrences, symbols, local_counter,
            );
        }
    }

    /// Walk a trait definition.
    fn walk_trait(
        &self,
        trait_def: &mago_syntax::ast::Trait<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let trait_name = self.identifier_name(&trait_def.name, source);
        let fqn = self.resolve_name_at(&trait_def.name, resolved_names, source);

        let trait_symbol = builder.class_like_symbol(&fqn);
        let trait_symbol_str = format_symbol(&trait_symbol);

        let name_span = trait_def.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: trait_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        symbols.push(SymbolInformation {
            symbol: trait_symbol_str,
            kind: types::symbol_information::Kind::Trait.into(),
            display_name: trait_name,
            ..Default::default()
        });

        for member in &trait_def.body.members {
            self.walk_class_member(
                member, &fqn, resolved_names, builder, line_index, source,
                occurrences, symbols, local_counter,
            );
        }
    }

    /// Walk an enum definition.
    fn walk_enum(
        &self,
        enum_def: &mago_syntax::ast::Enum<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let enum_name = self.identifier_name(&enum_def.name, source);
        let fqn = self.resolve_name_at(&enum_def.name, resolved_names, source);

        let enum_symbol = builder.class_like_symbol(&fqn);
        let enum_symbol_str = format_symbol(&enum_symbol);

        let name_span = enum_def.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: enum_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let mut relationships = Vec::new();
        if let Some(implements) = &enum_def.implements {
            for iface in &implements.types.inner {
                let iface_name = self.resolve_name_at(iface, resolved_names, source);
                let iface_sym = builder.class_like_symbol(&iface_name);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&iface_sym),
                    is_implementation: true,
                    ..Default::default()
                });
                let iface_span = iface.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(
                        iface_span.start.offset,
                        iface_span.end.offset,
                    ),
                    symbol: format_symbol(&iface_sym),
                    ..Default::default()
                });
            }
        }

        symbols.push(SymbolInformation {
            symbol: enum_symbol_str.clone(),
            kind: types::symbol_information::Kind::Enum.into(),
            display_name: enum_name,
            relationships,
            ..Default::default()
        });

        // Walk enum cases and other members
        for member in &enum_def.body.members {
            match member {
                mago_syntax::ast::EnumMember::Case(case) => {
                    self.walk_enum_case(
                        case, &fqn, builder, line_index, source,
                        occurrences, symbols,
                    );
                }
                mago_syntax::ast::EnumMember::Method(method) => {
                    self.walk_method(
                        method, &fqn, resolved_names, builder, line_index, source,
                        occurrences, symbols, local_counter,
                    );
                }
                mago_syntax::ast::EnumMember::Constant(constant) => {
                    self.walk_class_constant(
                        constant, &fqn, builder, line_index, source,
                        occurrences, symbols,
                    );
                }
                _ => {}
            }
        }
    }

    /// Walk an enum case.
    fn walk_enum_case(
        &self,
        case: &mago_syntax::ast::EnumCase<'_>,
        enum_fqn: &str,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        let case_name = self.identifier_name(&case.name, source);
        let case_symbol = builder.enum_case_symbol(enum_fqn, &case_name);
        let case_symbol_str = format_symbol(&case_symbol);

        let name_span = case.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: case_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        symbols.push(SymbolInformation {
            symbol: case_symbol_str,
            kind: types::symbol_information::Kind::EnumMember.into(),
            display_name: case_name,
            ..Default::default()
        });
    }

    /// Walk a class/interface/trait member.
    fn walk_class_member(
        &self,
        member: &mago_syntax::ast::ClassLikeMember<'_>,
        class_fqn: &str,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        use mago_syntax::ast::ClassLikeMember;
        match member {
            ClassLikeMember::Method(method) => {
                self.walk_method(
                    method, class_fqn, resolved_names, builder, line_index, source,
                    occurrences, symbols, local_counter,
                );
            }
            ClassLikeMember::Property(prop) => {
                self.walk_property(
                    prop, class_fqn, builder, line_index, source,
                    occurrences, symbols,
                );
            }
            ClassLikeMember::Constant(constant) => {
                self.walk_class_constant(
                    constant, class_fqn, builder, line_index, source,
                    occurrences, symbols,
                );
            }
            ClassLikeMember::TraitUse(trait_use) => {
                // Emit references for used traits
                for trait_name in &trait_use.trait_names.inner {
                    let fqn = self.resolve_name_at(trait_name, resolved_names, source);
                    let trait_sym = builder.class_like_symbol(&fqn);
                    let trait_span = trait_name.span();
                    occurrences.push(Occurrence {
                        range: line_index.scip_range(
                            trait_span.start.offset,
                            trait_span.end.offset,
                        ),
                        symbol: format_symbol(&trait_sym),
                        ..Default::default()
                    });
                }
            }
            _ => {}
        }
    }

    /// Walk a method definition.
    fn walk_method(
        &self,
        method: &mago_syntax::ast::Method<'_>,
        class_fqn: &str,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let method_name = self.identifier_name(&method.name, source);
        let method_symbol = builder.method_symbol(class_fqn, &method_name);
        let method_symbol_str = format_symbol(&method_symbol);

        let name_span = method.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: method_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let kind = if method_name == "__construct" {
            types::symbol_information::Kind::Constructor
        } else {
            types::symbol_information::Kind::Method
        };

        symbols.push(SymbolInformation {
            symbol: method_symbol_str,
            kind: kind.into(),
            display_name: method_name.clone(),
            ..Default::default()
        });

        // Walk parameters
        for param in &method.parameters.parameters.inner {
            let param_name = self.extract_variable_name(&param.variable, source);
            let param_sym = builder.parameter_symbol(class_fqn, &method_name, &param_name);
            let param_symbol_str = format_symbol(&param_sym);

            let param_span = param.variable.span();
            occurrences.push(Occurrence {
                range: line_index.scip_range(param_span.start.offset, param_span.end.offset),
                symbol: param_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: param_symbol_str,
                kind: types::symbol_information::Kind::Parameter.into(),
                display_name: format!("${}", param_name),
                ..Default::default()
            });

            // Emit reference for parameter type hint
            if let Some(type_hint) = &param.type_hint {
                self.walk_type_hint(
                    type_hint, resolved_names, builder, line_index, source, occurrences,
                );
            }
        }

        // Emit reference for return type
        if let Some(return_type) = &method.return_type_hint {
            self.walk_type_hint(
                &return_type.type_hint, resolved_names, builder, line_index, source,
                occurrences,
            );
        }
    }

    /// Walk a function definition (top-level).
    fn walk_function(
        &self,
        func: &mago_syntax::ast::Function<'_>,
        _enclosing_class: Option<&str>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let func_name = self.identifier_name(&func.name, source);
        let fqn = self.resolve_name_at(&func.name, resolved_names, source);

        let func_symbol = builder.function_symbol(&fqn);
        let func_symbol_str = format_symbol(&func_symbol);

        let name_span = func.name.span();
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: func_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        symbols.push(SymbolInformation {
            symbol: func_symbol_str,
            kind: types::symbol_information::Kind::Function.into(),
            display_name: func_name.clone(),
            ..Default::default()
        });

        // Walk parameters
        for param in &func.parameters.parameters.inner {
            let param_name = self.extract_variable_name(&param.variable, source);
            let param_sym = builder.function_parameter_symbol(&fqn, &param_name);
            let param_symbol_str = format_symbol(&param_sym);

            let param_span = param.variable.span();
            occurrences.push(Occurrence {
                range: line_index.scip_range(param_span.start.offset, param_span.end.offset),
                symbol: param_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: param_symbol_str,
                kind: types::symbol_information::Kind::Parameter.into(),
                display_name: format!("${}", param_name),
                ..Default::default()
            });

            if let Some(type_hint) = &param.type_hint {
                self.walk_type_hint(
                    type_hint, resolved_names, builder, line_index, source, occurrences,
                );
            }
        }

        if let Some(return_type) = &func.return_type_hint {
            self.walk_type_hint(
                &return_type.type_hint, resolved_names, builder, line_index, source,
                occurrences,
            );
        }
    }

    /// Walk a property definition.
    fn walk_property(
        &self,
        prop: &mago_syntax::ast::Property<'_>,
        class_fqn: &str,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        // Properties can have multiple variables: public $a, $b;
        for item in &prop.items.inner {
            let var_name = self.extract_variable_name(&item.variable, source);
            let prop_sym = builder.property_symbol(class_fqn, &var_name);
            let prop_symbol_str = format_symbol(&prop_sym);

            let var_span = item.variable.span();
            occurrences.push(Occurrence {
                range: line_index.scip_range(var_span.start.offset, var_span.end.offset),
                symbol: prop_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: prop_symbol_str,
                kind: types::symbol_information::Kind::Property.into(),
                display_name: format!("${}", var_name),
                ..Default::default()
            });
        }
    }

    /// Walk a class constant definition.
    fn walk_class_constant(
        &self,
        constant: &mago_syntax::ast::ClassLikeConstant<'_>,
        class_fqn: &str,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        for item in &constant.items.inner {
            let const_name = self.identifier_name(&item.name, source);
            let const_sym = builder.class_constant_symbol(class_fqn, &const_name);
            let const_symbol_str = format_symbol(&const_sym);

            let name_span = item.name.span();
            occurrences.push(Occurrence {
                range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
                symbol: const_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: const_symbol_str,
                kind: types::symbol_information::Kind::Constant.into(),
                display_name: const_name,
                ..Default::default()
            });
        }
    }

    /// Walk a top-level constant.
    fn walk_constant(
        &self,
        constant: &mago_syntax::ast::Constant<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        for item in &constant.items.inner {
            let const_name = self.identifier_name(&item.name, source);
            let fqn = self.resolve_name_at(&item.name, resolved_names, source);

            let const_sym = builder.constant_symbol(&fqn);
            let const_symbol_str = format_symbol(&const_sym);

            let name_span = item.name.span();
            occurrences.push(Occurrence {
                range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
                symbol: const_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: const_symbol_str,
                kind: types::symbol_information::Kind::Constant.into(),
                display_name: const_name,
                ..Default::default()
            });
        }
    }

    /// Walk a type hint to emit references to named types.
    fn walk_type_hint(
        &self,
        type_hint: &mago_syntax::ast::TypeHint<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
    ) {
        use mago_syntax::ast::TypeHint;
        match type_hint {
            TypeHint::Identifier(ident) => {
                // Named type like User, \App\Models\User, etc.
                // Skip built-in types
                let name = self.identifier_source_text(ident, source);
                if !is_builtin_type(&name) {
                    let fqn = self.resolve_name_at(ident, resolved_names, source);
                    let sym = builder.class_like_symbol(&fqn);
                    let span = ident.span();
                    occurrences.push(Occurrence {
                        range: line_index.scip_range(span.start.offset, span.end.offset),
                        symbol: format_symbol(&sym),
                        ..Default::default()
                    });
                }
            }
            TypeHint::Nullable(nullable) => {
                self.walk_type_hint(&nullable.type_hint, resolved_names, builder, line_index, source, occurrences);
            }
            TypeHint::Union(union) => {
                self.walk_type_hint(&union.left, resolved_names, builder, line_index, source, occurrences);
                self.walk_type_hint(&union.right, resolved_names, builder, line_index, source, occurrences);
            }
            TypeHint::Intersection(intersection) => {
                self.walk_type_hint(&intersection.left, resolved_names, builder, line_index, source, occurrences);
                self.walk_type_hint(&intersection.right, resolved_names, builder, line_index, source, occurrences);
            }
            _ => {
                // void, bool, int, string, etc. — no reference needed
            }
        }
    }

    // --- Helper methods ---

    /// Get the source text for an identifier.
    fn identifier_name<T: HasSpan>(&self, node: &T, source: &str) -> String {
        let span = node.span();
        source[span.start.offset as usize..span.end.offset as usize].to_string()
    }

    /// Get identifier source text (same as identifier_name but for type hints).
    fn identifier_source_text<T: HasSpan>(&self, node: &T, source: &str) -> String {
        self.identifier_name(node, source)
    }

    /// Resolve a name using the resolved names map, falling back to source text.
    fn resolve_name_at<T: mago_span::HasPosition>(
        &self,
        node: &T,
        resolved_names: &mago_names::ResolvedNames<'_>,
        source: &str,
    ) -> String {
        let pos = node.position();
        if let Some((fqn, _imported)) = resolved_names.get(&pos) {
            fqn.to_string()
        } else {
            // Fall back to source text
            // This handles cases where name resolution doesn't have the name
            // (e.g., for simple identifiers that aren't imports)
            let span = if let Some(spannable) = (node as &dyn std::any::Any).downcast_ref::<mago_span::Span>() {
                *spannable
            } else {
                return String::new();
            };
            source[span.start.offset as usize..span.end.offset as usize].to_string()
        }
    }

    /// Extract variable name without the `$` prefix.
    fn extract_variable_name<T: HasSpan>(&self, node: &T, source: &str) -> String {
        let name = self.identifier_name(node, source);
        name.strip_prefix('$').unwrap_or(&name).to_string()
    }
}

/// Check if a type name is a PHP built-in type.
fn is_builtin_type(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "int" | "float" | "string" | "bool" | "boolean" | "array" | "object"
            | "null" | "void" | "never" | "mixed" | "callable" | "iterable"
            | "self" | "static" | "parent" | "true" | "false" | "resource"
    )
}
```

**Note:** This initial implementation focuses on **definitions** (classes, interfaces, traits, enums, functions, methods, properties, constants, parameters) and **type hint references**. Cross-file reference tracking (e.g., method calls, property accesses, function calls in expression context) will be added in Task 7.

**Step 3: Update src/main.rs to use the indexer**

```rust
use std::path::Path;

use anyhow::Result;
use clap::Parser;

use scip_php::indexer::Indexer;
use scip_php::project::PhpProject;

#[derive(Parser, Debug)]
#[command(name = "scip-php", about = "SCIP indexer for PHP")]
struct Args {
    /// Path to the PHP project root (containing composer.json)
    #[arg(default_value = ".")]
    project_root: String,

    /// Output file path
    #[arg(short, long, default_value = "index.scip")]
    output: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let project = PhpProject::discover(Path::new(&args.project_root))?;
    eprintln!(
        "scip-php: project '{}' v{} at {}",
        project.package.name,
        project.package.version,
        project.root.display()
    );

    let indexer = Indexer::new(project);
    let index = indexer.index()?;

    scip::write_message_to_file(&args.output, index)?;
    eprintln!("scip-php: wrote {}", args.output);

    Ok(())
}
```

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: May need adjustments depending on exact Mago API. Fix type mismatches as needed.

**Step 5: Commit**

```bash
git add src/indexer.rs src/main.rs src/lib.rs
git commit -m "feat: implement core SCIP indexer with definition tracking"
```

---

## Task 6: Integration Test with Sample PHP Project

**Files:**
- Create: `tests/fixtures/sample-project/composer.json`
- Create: `tests/fixtures/sample-project/src/Models/User.php`
- Create: `tests/fixtures/sample-project/src/Contracts/UserRepository.php`
- Create: `tests/fixtures/sample-project/src/Enums/Status.php`
- Create: `tests/integration_test.rs`

**Step 1: Create sample PHP project**

`tests/fixtures/sample-project/composer.json`:
```json
{
    "name": "test/sample-project",
    "version": "1.0.0",
    "autoload": {
        "psr-4": {
            "App\\": "src/"
        }
    }
}
```

`tests/fixtures/sample-project/src/Contracts/UserRepository.php`:
```php
<?php

namespace App\Contracts;

interface UserRepository
{
    public function find(int $id): ?User;
    public function save(User $user): void;
}
```

`tests/fixtures/sample-project/src/Models/User.php`:
```php
<?php

namespace App\Models;

use App\Contracts\UserRepository;

class User
{
    public const MAX_NAME_LENGTH = 255;

    private string $name;
    private int $age;

    public function __construct(string $name, int $age)
    {
        $this->name = $name;
        $this->age = $age;
    }

    public function getName(): string
    {
        return $this->name;
    }

    public function setName(string $name): void
    {
        $this->name = $name;
    }
}
```

`tests/fixtures/sample-project/src/Enums/Status.php`:
```php
<?php

namespace App\Enums;

enum Status: string
{
    case Active = 'active';
    case Inactive = 'inactive';
    case Pending = 'pending';
}
```

**Step 2: Create integration test**

`tests/integration_test.rs`:
```rust
use std::path::Path;

use scip_php::indexer::Indexer;
use scip_php::project::PhpProject;

#[test]
fn test_index_sample_project() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    assert_eq!(project.package.name, "test/sample-project");
    assert_eq!(project.package.version, "1.0.0");

    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    // Should have metadata
    assert!(index.metadata.is_some());
    let metadata = index.metadata.as_ref().unwrap();
    assert!(metadata.tool_info.is_some());
    assert_eq!(metadata.tool_info.as_ref().unwrap().name, "scip-php");

    // Should have documents for each PHP file
    assert!(!index.documents.is_empty());

    // Find the User document
    let user_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("User.php"))
        .expect("Should have a User.php document");

    assert_eq!(user_doc.language, "PHP");

    // Should have occurrences for class, methods, properties
    assert!(!user_doc.occurrences.is_empty());

    // Check that User class definition exists
    let user_def = user_doc
        .occurrences
        .iter()
        .find(|o| {
            o.symbol.contains("User#")
                && !o.symbol.contains("UserRepository")
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
        })
        .expect("Should have User class definition");

    assert!(user_def.symbol.contains("test/sample-project"));
    assert!(user_def.symbol.contains("App/Models/User#"));

    // Check that Status enum document exists
    let status_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("Status.php"))
        .expect("Should have a Status.php document");

    // Should have enum case definitions
    let active_case = status_doc
        .occurrences
        .iter()
        .find(|o| o.symbol.contains("Active") && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0)
        .expect("Should have Active enum case definition");

    assert!(active_case.symbol.contains("App/Enums/Status#Active."));

    // Find the UserRepository interface document
    let repo_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("UserRepository.php"))
        .expect("Should have a UserRepository.php document");

    // Should have interface definition
    let repo_def = repo_doc
        .occurrences
        .iter()
        .find(|o| {
            o.symbol.contains("UserRepository#")
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
        })
        .expect("Should have UserRepository interface definition");

    assert!(repo_def.symbol.contains("App/Contracts/UserRepository#"));
}

#[test]
fn test_scip_output_file() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    // Write to a temp file and verify it can be read back
    let output_path = std::env::temp_dir().join("test-scip-php.scip");
    scip::write_message_to_file(output_path.to_str().unwrap(), index).unwrap();

    assert!(output_path.exists());
    let file_size = std::fs::metadata(&output_path).unwrap().len();
    assert!(file_size > 0, "SCIP file should not be empty");

    // Clean up
    let _ = std::fs::remove_file(&output_path);
}
```

**Step 3: Run tests**

Run: `cargo test`
Expected: All tests pass. If any Mago API mismatches arise, fix them.

**Step 4: Commit**

```bash
git add tests/
git commit -m "test: add integration tests with sample PHP project"
```

---

## Task 7: Expression Reference Tracking (Phase 2)

This task adds reference tracking for expressions — method calls, property accesses, function calls, `new` instantiation, `use` imports, and variable references in code bodies. This significantly improves the usefulness of the SCIP index.

**Files:**
- Modify: `src/indexer.rs` (add expression walking)

**This is the most complex task.** The approach:
1. Walk all expression nodes in method/function bodies
2. For `new ClassName()` — emit reference to the class
3. For `$obj->method()` — emit reference (requires type info from codex, deferred)
4. For `ClassName::method()` — emit static method reference
5. For `use App\Models\User` — emit import reference
6. For function calls `functionName()` — emit function reference

**Step 1: Add expression walking methods to Indexer**

Add these methods to the `Indexer` impl block in `src/indexer.rs`:

```rust
    /// Walk an expression to find references.
    fn walk_expression(
        &self,
        expr: &mago_syntax::ast::Expression<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        local_counter: &mut usize,
    ) {
        use mago_syntax::ast::Expression;
        match expr {
            // new ClassName(...)
            Expression::Instantiation(inst) => {
                self.walk_expression(&inst.class, resolved_names, builder, line_index, source, occurrences, local_counter);
                for arg in &inst.arguments.arguments.inner {
                    self.walk_expression(&arg.value, resolved_names, builder, line_index, source, occurrences, local_counter);
                }
            }
            // ClassName::method() or ClassName::$prop
            Expression::StaticMethodCall(call) => {
                // Reference to the class
                self.walk_expression(&call.class, resolved_names, builder, line_index, source, occurrences, local_counter);
                // Arguments
                for arg in &call.arguments.arguments.inner {
                    self.walk_expression(&arg.value, resolved_names, builder, line_index, source, occurrences, local_counter);
                }
            }
            // functionName(...)
            Expression::FunctionCall(call) => {
                self.walk_expression(&call.function, resolved_names, builder, line_index, source, occurrences, local_counter);
                for arg in &call.arguments.arguments.inner {
                    self.walk_expression(&arg.value, resolved_names, builder, line_index, source, occurrences, local_counter);
                }
            }
            // Identifier (class name, function name references)
            Expression::Identifier(ident) => {
                let name = self.identifier_source_text(ident, source);
                if !is_builtin_type(&name) {
                    let fqn = self.resolve_name_at(ident, resolved_names, source);
                    // Could be a class reference or function reference
                    let sym = builder.class_like_symbol(&fqn);
                    let span = ident.span();
                    occurrences.push(Occurrence {
                        range: line_index.scip_range(span.start.offset, span.end.offset),
                        symbol: format_symbol(&sym),
                        ..Default::default()
                    });
                }
            }
            // $variable — local variable reference
            Expression::Variable(var) => {
                // Emit as local symbol reference
                // TODO: Implement proper local variable tracking
                // with scope-aware local counter
            }
            _ => {
                // TODO: Handle more expression types:
                // - $obj->method() (MethodCall)
                // - $obj->prop (PropertyAccess)
                // - ClassName::CONST (StaticPropertyAccess)
                // - Array access, closure, arrow function, etc.
            }
        }
    }

    /// Walk use/import statements.
    fn walk_use_statement(
        &self,
        use_stmt: &mago_syntax::ast::Use<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
    ) {
        for item in &use_stmt.items.inner {
            let name = self.identifier_name(&item.name, source);
            let fqn = self.resolve_name_at(&item.name, resolved_names, source);

            // Determine if it's a class, function, or constant import
            // based on the use statement kind
            let sym = builder.class_like_symbol(&fqn);
            let span = item.name.span();
            occurrences.push(Occurrence {
                range: line_index.scip_range(span.start.offset, span.end.offset),
                symbol: format_symbol(&sym),
                symbol_roles: scip::types::SymbolRole::Import as i32,
                ..Default::default()
            });
        }
    }
```

**Step 2: Wire expression walking into method/function bodies**

Update `walk_method` and `walk_function` to also walk their body statements for expressions.

**Step 3: Add `use` statement handling to `walk_statements`**

Add to the match arm in `walk_statements`:
```rust
Statement::Use(use_stmt) => {
    self.walk_use_statement(
        use_stmt, resolved_names, builder, line_index, source, occurrences,
    );
}
```

**Step 4: Run tests**

Run: `cargo test`
Expected: All tests still pass, and new reference occurrences are emitted.

**Step 5: Commit**

```bash
git add src/indexer.rs
git commit -m "feat: add expression reference tracking for imports, instantiation, and calls"
```

---

## Task 8: Documentation Extraction (PHPDoc)

Extract documentation from PHPDoc comments and attach to SymbolInformation.

**Files:**
- Modify: `src/indexer.rs`

**Step 1: Add docblock extraction helper**

Add to the Indexer:

```rust
    /// Extract documentation from a docblock attached to a definition.
    fn extract_documentation(
        &self,
        trivia: &mago_syntax::Trivia<'_>,
        span: &mago_span::Span,
        source: &str,
    ) -> Vec<String> {
        let arena = bumpalo::Bump::new();
        let docs = mago_docblock::parse_trivia(&arena, trivia, span);

        let mut documentation = Vec::new();
        for doc in docs {
            // Extract the description text from the docblock
            let mut text = String::new();
            for element in &doc.elements {
                match element {
                    mago_docblock::document::Element::Text(segments) => {
                        for segment in segments {
                            match segment {
                                mago_docblock::document::TextSegment::Plain(s) => {
                                    text.push_str(s);
                                }
                                mago_docblock::document::TextSegment::Code(s) => {
                                    text.push('`');
                                    text.push_str(s);
                                    text.push('`');
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !text.trim().is_empty() {
                documentation.push(text.trim().to_string());
            }
        }
        documentation
    }
```

**Step 2: Wire documentation into definition emission**

Update class, method, function, property definitions to include `documentation` field.

**Step 3: Run tests**

Run: `cargo test`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add src/indexer.rs
git commit -m "feat: extract PHPDoc documentation for symbol hover info"
```

---

## Task 9: CLI Polish and Error Handling

**Files:**
- Modify: `src/main.rs`

**Step 1: Add timing and summary output**

```rust
use std::time::Instant;

fn main() -> Result<()> {
    let args = Args::parse();
    let start = Instant::now();

    let project = PhpProject::discover(Path::new(&args.project_root))?;
    eprintln!(
        "scip-php: project '{}' v{} at {}",
        project.package.name,
        project.package.version,
        project.root.display()
    );

    let indexer = Indexer::new(project);
    let index = indexer.index()?;

    let doc_count = index.documents.len();
    let occ_count: usize = index.documents.iter().map(|d| d.occurrences.len()).sum();
    let sym_count: usize = index.documents.iter().map(|d| d.symbols.len()).sum();

    scip::write_message_to_file(&args.output, index)?;

    let elapsed = start.elapsed();
    eprintln!(
        "scip-php: indexed {} documents, {} occurrences, {} symbols in {:.2}s → {}",
        doc_count,
        occ_count,
        sym_count,
        elapsed.as_secs_f64(),
        args.output,
    );

    Ok(())
}
```

**Step 2: Build release binary and test on a real project**

Run: `cargo build --release`
Run: `./target/release/scip-php /path/to/real/php/project -o index.scip`
Expected: Produces index.scip file with meaningful content.

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add timing and summary stats to CLI output"
```

---

## Task 10: Validate SCIP Output

Use the `scip` CLI tool to validate the generated index.

**Step 1: Install the SCIP CLI**

Run: `go install github.com/sourcegraph/scip/cmd/scip@latest`

Or download from GitHub releases.

**Step 2: Validate the output**

Run: `scip print --json index.scip | head -100`
Expected: Valid JSON output showing documents, occurrences, and symbols.

Run: `scip stats index.scip`
Expected: Summary statistics matching our CLI output.

Run: `scip snapshot --from index.scip --to snapshot/`
Expected: Human-readable snapshot files.

**Step 3: Fix any issues found during validation**

**Step 4: Commit any fixes**

```bash
git commit -am "fix: address SCIP validation issues"
```

---

## Future Tasks (Not in Initial MVP)

These are tracked for future iterations:

### Phase 2 — Enhanced References
- **Local variable tracking**: Track `$variable` definitions and references within function/method bodies using scope analysis
- **Method call resolution**: For `$obj->method()`, use Mago's codex type inference to determine the class and emit proper method references
- **Property access resolution**: For `$obj->prop`, determine class and emit property references
- **Static member access**: `ClassName::method()`, `ClassName::$prop`, `ClassName::CONST`
- **String class references**: Handle `ClassName::class` expressions

### Phase 3 — Advanced Features
- **Trait method resolution**: Track which trait provides each method
- **Magic method references**: `__get`, `__set`, `__call`, `__callStatic`
- **Anonymous classes and closures**: Assign unique symbols
- **PHPDoc type references**: `@param ClassName $var`, `@return ClassName`
- **Generic/template types**: PHPStan/Psalm `@template T` annotations
- **Cross-package references**: Index vendor dependencies and emit external_symbols
- **Incremental indexing**: Only re-index changed files
- **Parallel file processing**: Use rayon for parallel parsing

### Phase 4 — Production Readiness
- **Performance benchmarking** against large codebases (Laravel, Symfony)
- **Memory optimization**: Streaming document emission for huge projects
- **CI integration**: GitHub Actions for testing
- **Release binaries**: Cross-platform builds
- **Composer plugin**: `composer require --dev scip-php/scip-php`
