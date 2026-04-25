//! Command macros — `/macro define|run|list|delete`.

use std::collections::HashMap;
use std::path::PathBuf;

/// Registry of user-defined command macros.
#[derive(Debug, Default)]
pub struct MacroRegistry {
    macros: HashMap<String, Vec<String>>,
    file_path: Option<PathBuf>,
}

impl MacroRegistry {
    /// Create a new macro registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            macros: HashMap::new(),
            file_path: None,
        }
    }

    /// Set the file path for persistence.
    pub fn set_file_path(&mut self, path: PathBuf) {
        self.file_path = Some(path);
    }

    /// Define a new macro.
    pub fn define(&mut self, name: &str, commands: Vec<String>) {
        self.macros.insert(name.to_string(), commands);
        self.save_if_configured();
    }

    /// Delete a macro by name.
    pub fn delete(&mut self, name: &str) -> bool {
        let removed = self.macros.remove(name).is_some();
        if removed {
            self.save_if_configured();
        }
        removed
    }

    /// Get the commands for a macro, if it exists.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[String]> {
        self.macros.get(name).map(|v| v.as_slice())
    }

    /// List all macro names.
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.macros.keys().cloned().collect();
        names.sort();
        names
    }

    /// Check if a macro exists.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.macros.contains_key(name)
    }

    /// Load macros from the configured file.
    pub fn load(&mut self) -> std::io::Result<()> {
        let Some(ref path) = self.file_path else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let content = std::fs::read_to_string(path)?;
        let map: HashMap<String, Vec<String>> = serde_json::from_str(&content)?;
        self.macros = map;
        Ok(())
    }

    /// Save macros to the configured file.
    fn save_if_configured(&self) {
        let Some(ref path) = self.file_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let content = serde_json::to_string_pretty(&self.macros).unwrap_or_default();
        let _ = std::fs::write(path, content);
    }

    /// Get the total number of macros.
    #[must_use]
    pub fn len(&self) -> usize {
        self.macros.len()
    }

    /// Check if no macros are defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.macros.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn define_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = MacroRegistry::new();
        reg.set_file_path(tmp.path().join("macros.json"));

        reg.define(
            "deploy",
            vec!["compact".into(), "commit".into(), "exit".into()],
        );
        assert!(reg.has("deploy"));
        assert_eq!(reg.get("deploy").unwrap().len(), 3);
    }

    #[test]
    fn delete_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = MacroRegistry::new();
        reg.set_file_path(tmp.path().join("macros.json"));

        reg.define("test", vec!["status".into()]);
        assert!(reg.delete("test"));
        assert!(!reg.has("test"));
        assert!(!reg.delete("nonexistent"));
    }

    #[test]
    fn list_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = MacroRegistry::new();
        reg.set_file_path(tmp.path().join("macros.json"));

        reg.define("zebra", vec!["a".into()]);
        reg.define("alpha", vec!["b".into()]);
        reg.define("middle", vec!["c".into()]);

        let names = reg.list();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn load_save_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("macros.json");

        let mut reg1 = MacroRegistry::new();
        reg1.set_file_path(path.clone());
        reg1.define("run", vec!["status".into(), "commit".into()]);

        let mut reg2 = MacroRegistry::new();
        reg2.set_file_path(path);
        reg2.load().unwrap();
        assert!(reg2.has("run"));
        assert_eq!(reg2.get("run").unwrap(), &["status", "commit"]);
    }

    #[test]
    fn load_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("macros.json");
        std::fs::write(&path, "{}").unwrap();

        let mut reg = MacroRegistry::new();
        reg.set_file_path(path);
        reg.load().unwrap();
        assert!(reg.is_empty());
    }
}
