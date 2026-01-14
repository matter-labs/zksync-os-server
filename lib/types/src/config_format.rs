use std::path::Path;

/// Supported configuration file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Yaml,
}

impl ConfigFormat {
    /// Detects the configuration format from a file path based on its extension.
    ///
    /// # Panics
    /// Panics if the file extension is not supported (.json, .yaml, or .yml).
    pub fn from_path(path: &Path) -> Self {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_lowercase());

        match extension.as_deref() {
            Some("yaml") | Some("yml") => ConfigFormat::Yaml,
            Some("json") => ConfigFormat::Json,
            _ => {
                panic!(
                    "Unsupported config file extension for path '{}'. Supported extensions are .json, .yaml and .yml",
                    path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_extension() {
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.json")),
            ConfigFormat::Json
        );
    }

    #[test]
    fn test_yaml_extension() {
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.yaml")),
            ConfigFormat::Yaml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.yml")),
            ConfigFormat::Yaml
        );
    }

    #[test]
    fn test_case_insensitive() {
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.JSON")),
            ConfigFormat::Json
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.YAML")),
            ConfigFormat::Yaml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.YML")),
            ConfigFormat::Yaml
        );
    }

    #[test]
    #[should_panic(expected = "Unsupported config file extension")]
    fn test_unsupported_extension() {
        ConfigFormat::from_path(Path::new("config.toml"));
    }
}
