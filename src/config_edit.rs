//! Comment-preserving config.toml editing via toml_edit — shared by the
//! settings app backend (extracted from the retired TUI).

use anyhow::Context;
use std::path::{Path, PathBuf};

pub const MODELS: &[(&str, &str)] = &[
    ("base.en", "0.7s"),
    ("distil-small.en", "1.5s"),
    ("small.en", "1.9s"),
];

pub struct ConfigDoc {
    path: PathBuf,
    pub doc: toml_edit::DocumentMut,
}

impl ConfigDoc {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Self { path: path.to_path_buf(), doc: text.parse().context("config.toml parse failed")? })
    }

    pub fn str_at(&self, path: &[&str], default: &str) -> String {
        self.item_at(path).and_then(|i| i.as_str()).unwrap_or(default).to_string()
    }
    pub fn bool_at(&self, path: &[&str], default: bool) -> bool {
        self.item_at(path).and_then(|i| i.as_bool()).unwrap_or(default)
    }
    pub fn int_at(&self, path: &[&str], default: i64) -> i64 {
        self.item_at(path).and_then(|i| i.as_integer()).unwrap_or(default)
    }
    fn item_at(&self, path: &[&str]) -> Option<&toml_edit::Item> {
        let mut item: &toml_edit::Item = self.doc.as_item();
        for key in path {
            item = item.get(key)?;
        }
        Some(item)
    }

    pub fn set(&mut self, path: &[&str], value: toml_edit::Value) {
        let mut item = self.doc.as_item_mut();
        for key in &path[..path.len() - 1] {
            if item.get(key).is_none() {
                item[key] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            item = &mut item[key];
        }
        item[path[path.len() - 1]] = toml_edit::value(value);
    }

    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::write(&self.path, self.doc.to_string())
            .with_context(|| format!("cannot write {}", self.path.display()))
    }
}
