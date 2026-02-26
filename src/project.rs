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
            .as_ref()
            .map(|n| n.0.as_str())
            .unwrap_or(".")
            .to_string();

        let version = composer
            .version
            .as_ref()
            .map(|v| v.0.as_str())
            .unwrap_or("dev")
            .to_string();

        // Collect source directories from PSR-4 autoload
        let mut source_dirs = Vec::new();

        if let Some(autoload) = &composer.autoload {
            for (_namespace, paths) in &autoload.psr_4 {
                match paths {
                    mago_composer::AutoloadPsr4value::String(p) => {
                        let dir = root.join(p);
                        if dir.exists() {
                            source_dirs.push(dir);
                        }
                    }
                    mago_composer::AutoloadPsr4value::Array(ps) => {
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
                    mago_composer::ComposerPackageAutoloadDevPsr4value::String(p) => {
                        let dir = root.join(p);
                        if dir.exists() {
                            source_dirs.push(dir);
                        }
                    }
                    mago_composer::ComposerPackageAutoloadDevPsr4value::Array(ps) => {
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
            for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
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
