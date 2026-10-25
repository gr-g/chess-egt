//! Location parsing and the async boundary for synchronous tablebase operations.
//!
//! Each root requires an immutable `format.json`: format `chess-egt`, format,
//! indexing and layout versions 1, and 262144 outcomes per frame. Generation
//! creates it conditionally; readers only validate it. Table data and optional
//! statistics use `TableId::relative_path("ggegt")` and `relative_path("json")`.
//! Neither probing nor format validation requires statistics.

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};

use object_store::{
    ObjectStore, ObjectStoreExt, ObjectStoreScheme, PutMode, PutOptions, path::Path as ObjectPath,
};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use url::Url;

use crate::{EgtError, EgtResult};

pub(crate) const FORMAT_VERSION: u32 = 1;
pub(crate) const INDEXING_VERSION: u32 = 1;
pub(crate) const LAYOUT_VERSION: u32 = 1;
pub(crate) const DEFAULT_FRAME_SIZE: usize = 256 * 1024;

/// Root metadata is immutable and independent of optional per-table statistics.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FormatDescriptor {
    format: String,
    format_version: u32,
    indexing_version: u32,
    layout_version: u32,
    frame_size: usize,
}

impl Default for FormatDescriptor {
    fn default() -> Self {
        Self {
            format: "chess-egt".into(),
            format_version: FORMAT_VERSION,
            indexing_version: INDEXING_VERSION,
            layout_version: LAYOUT_VERSION,
            frame_size: DEFAULT_FRAME_SIZE,
        }
    }
}

/// A shared backend and a base prefix.
#[derive(Debug, Clone)]
pub(crate) struct Location {
    pub(crate) store: Arc<dyn ObjectStore>,
    pub(crate) prefix: ObjectPath,
    pub(crate) display: String,
    pub(crate) writable: bool,
    // Shared clones trust the immutable root after successful validation.
    format_validated: Arc<OnceLock<()>>,
}

impl Location {
    pub(crate) fn parse(path: impl AsRef<Path>) -> EgtResult<Self> {
        let path = path.as_ref();
        let url = if let Some(text) = path.to_str().filter(|text| text.contains("://")) {
            Url::parse(text).map_err(|error| EgtError::InvalidStorageLocation(error.to_string()))?
        } else {
            let absolute = std::path::absolute(path)?;
            Url::from_directory_path(absolute).map_err(|_| {
                EgtError::InvalidStorageLocation("cannot represent local directory as a URL".into())
            })?
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(EgtError::InvalidStorageLocation(
                "URL userinfo is unsupported; configure credentials through environment variables"
                    .into(),
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(EgtError::InvalidStorageLocation(
                "URL queries and fragments are unsupported".into(),
            ));
        }
        let display = url.to_string();
        let (scheme, _) = ObjectStoreScheme::parse(&url).map_err(|source| EgtError::Storage {
            location: display.clone(),
            source: source.into(),
        })?;
        let writable = scheme != ObjectStoreScheme::Http;
        // parse_url alone does not load environment configuration. The opts
        // variant recognizes backend-specific keys and ignores unrelated ones.
        let options = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)));
        let (store, prefix) =
            object_store::parse_url_opts(&url, options).map_err(|source| EgtError::Storage {
                location: display.clone(),
                source,
            })?;
        Ok(Self {
            store: Arc::from(store),
            prefix,
            display,
            writable,
            format_validated: Arc::new(OnceLock::new()),
        })
    }

    /// Builds a hierarchical key from a slash-separated relative path.
    pub(crate) fn key(&self, relative: &str) -> ObjectPath {
        // ObjectPath::join accepts one segment and escapes embedded slashes.
        relative
            .split('/')
            .fold(self.prefix.clone(), |key, segment| key.join(segment))
    }

    pub(crate) fn describe(&self, key: &ObjectPath) -> String {
        let suffix = key
            .as_ref()
            .strip_prefix(self.prefix.as_ref())
            .unwrap_or(key.as_ref())
            .trim_start_matches('/');
        format!("{}/{}", self.display.trim_end_matches('/'), suffix)
    }

    pub(crate) fn error(&self, key: &ObjectPath, source: object_store::Error) -> EgtError {
        let location = self.describe(key);
        if matches!(source, object_store::Error::NotFound { .. }) {
            EgtError::FileNotFound(location)
        } else {
            EgtError::Storage { location, source }
        }
    }

    /// Validates an existing root descriptor without creating or modifying objects.
    /// Success is shared across clones; errors are never cached. Roots must remain
    /// immutable while a Location is in use, including after this validation.
    pub(crate) fn validate_format(&self) -> EgtResult<()> {
        if self.format_validated.get().is_some() {
            return Ok(());
        }
        let key = self.prefix.clone().join("format.json");
        let result = block_on(self.store.get(&key)).map_err(|error| self.error(&key, error))?;
        let bytes = block_on(result.bytes()).map_err(|error| self.error(&key, error))?;
        let actual: FormatDescriptor = serde_json::from_slice(&bytes)?;
        if actual != FormatDescriptor::default() {
            return Err(EgtError::InvalidStorageLocation(format!(
                "unsupported tablebase format at {}: {:?}",
                self.describe(&key),
                actual
            )));
        }
        let _ = self.format_validated.set(());
        Ok(())
    }

    /// Creates the descriptor once; concurrent generators never overwrite metadata.
    pub(crate) fn ensure_format(&self) -> EgtResult<()> {
        self.ensure_writable()?;
        match self.validate_format() {
            Ok(()) => return Ok(()),
            Err(EgtError::FileNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let key = self.prefix.clone().join("format.json");
        let bytes = serde_json::to_vec_pretty(&FormatDescriptor::default())?;
        let options = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match block_on(self.store.put_opts(&key, bytes.into(), options)) {
            Ok(_) => {
                let _ = self.format_validated.set(());
                Ok(())
            }
            Err(object_store::Error::AlreadyExists { .. }) => self.validate_format(),
            Err(error) => Err(self.error(&key, error)),
        }
    }

    pub(crate) fn ensure_writable(&self) -> EgtResult<()> {
        if !self.writable {
            return Err(EgtError::InvalidStorageLocation(format!(
                "HTTP storage is read-only: {}",
                self.display
            )));
        }
        Ok(())
    }
}

// Independent runtime workers keep network progress running while Rayon workers
// wait. Synchronous callers inside an async application should use spawn_blocking.
static IO_RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to create tablebase I/O runtime")
});

pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    IO_RUNTIME.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_required_immutable_and_read_only_when_opened() {
        let location = Location::parse("memory:///tables").unwrap();
        let key = location.prefix.clone().join("format.json");
        assert!(matches!(
            location.validate_format(),
            Err(EgtError::FileNotFound(_))
        ));
        assert!(matches!(
            block_on(location.store.head(&key)),
            Err(object_store::Error::NotFound { .. })
        ));
        location.ensure_format().unwrap();
        let before = block_on(location.store.get(&key)).unwrap();
        let before = block_on(before.bytes()).unwrap();
        location.ensure_format().unwrap();
        let read_only = Location {
            writable: false,
            ..location.clone()
        };
        read_only.validate_format().unwrap();
        assert!(read_only.ensure_format().is_err());
        let after = block_on(location.store.get(&key)).unwrap();
        assert_eq!(before, block_on(after.bytes()).unwrap());
        let actual: serde_json::Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(
            actual,
            serde_json::json!({
                "format": "chess-egt", "format_version": 1, "indexing_version": 1,
                "layout_version": 1, "frame_size": 262144
            })
        );
    }

    #[test]
    fn format_cache_is_shared_and_only_remembers_success() {
        let location = Location::parse("memory:///tables").unwrap();
        let clone = location.clone();
        let key = location.prefix.clone().join("format.json");
        assert!(Arc::ptr_eq(
            &location.format_validated,
            &clone.format_validated
        ));
        assert!(matches!(
            clone.validate_format(),
            Err(EgtError::FileNotFound(_))
        ));
        assert!(location.format_validated.get().is_none());

        block_on(location.store.put(&key, b"not json".to_vec().into())).unwrap();
        assert!(matches!(clone.validate_format(), Err(EgtError::Json(_))));
        assert!(matches!(clone.ensure_format(), Err(EgtError::Json(_))));
        assert!(location.format_validated.get().is_none());

        let bytes = serde_json::to_vec(&FormatDescriptor::default()).unwrap();
        block_on(location.store.put(&key, bytes.into())).unwrap();
        clone.validate_format().unwrap();
        assert!(location.format_validated.get().is_some());

        // Deliberately violate immutability: cached clones must not refetch it.
        block_on(location.store.put(&key, b"not json".to_vec().into())).unwrap();
        location.validate_format().unwrap();
        clone.validate_format().unwrap();
        let fresh = Location {
            format_validated: Arc::new(OnceLock::new()),
            ..location
        };
        assert!(matches!(fresh.validate_format(), Err(EgtError::Json(_))));
        assert!(fresh.format_validated.get().is_none());
    }

    #[test]
    fn incompatible_and_malformed_formats_are_never_replaced() {
        let location = Location::parse("memory:///tables").unwrap();
        location.ensure_format().unwrap();
        let key = location.prefix.clone().join("format.json");
        for field in [
            "format",
            "format_version",
            "indexing_version",
            "layout_version",
            "frame_size",
        ] {
            let mut descriptor = serde_json::to_value(FormatDescriptor::default()).unwrap();
            descriptor[field] = if field == "format" {
                serde_json::json!("other")
            } else {
                serde_json::json!(2)
            };
            let bytes = serde_json::to_vec(&descriptor).unwrap();
            block_on(location.store.put(&key, bytes.clone().into())).unwrap();
            let location = Location {
                format_validated: Arc::new(OnceLock::new()),
                ..location.clone()
            };
            assert!(matches!(
                location.validate_format(),
                Err(EgtError::InvalidStorageLocation(_))
            ));
            assert!(location.ensure_format().is_err());
            let result = block_on(location.store.get(&key)).unwrap();
            assert_eq!(block_on(result.bytes()).unwrap().as_ref(), bytes);
        }
        block_on(location.store.put(&key, b"not json".to_vec().into())).unwrap();
        let location = Location {
            format_validated: Arc::new(OnceLock::new()),
            ..location
        };
        assert!(location.validate_format().is_err());
        assert!(location.ensure_format().is_err());
    }

    #[test]
    fn concurrent_format_creation() {
        let location = Location::parse("memory:///tables").unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let location = &location;
                scope.spawn(move || location.ensure_format().unwrap());
            }
        });
        location.validate_format().unwrap();
    }

    #[test]
    fn local_paths_and_file_urls() {
        let relative = Location::parse("target/storage path/not-created").unwrap();
        let url = Location::parse(&relative.display).unwrap();
        assert_eq!(relative.prefix, url.prefix);
        assert!(relative.display.starts_with("file:///"));
        assert!(
            relative
                .prefix
                .as_ref()
                .ends_with("target/storage path/not-created")
        );
        relative.ensure_writable().unwrap();
        assert_eq!(
            relative.describe(&relative.prefix.clone().join("K_K.ggegt")),
            format!("{}K_K.ggegt", relative.display)
        );
    }

    #[test]
    fn literal_local_names() {
        for path in ["~/tables", "~other/tables", "tables?query#fragment"] {
            let location = Location::parse(path).unwrap();
            assert_eq!(
                Url::parse(&location.display)
                    .unwrap()
                    .to_file_path()
                    .unwrap(),
                std::path::absolute(path).unwrap()
            );
            location.ensure_writable().unwrap();
        }
    }

    #[test]
    fn unsupported_url_components() {
        for text in [
            "https://user:secret@example.com/tables",
            "https://user@example.com/tables",
            "https://:secret@example.com/tables",
            "s3://user:secret@bucket/tables",
            "memory:///tables?query",
            "memory:///tables?",
            "memory:///tables#fragment",
            "memory:///tables#",
            "unknown://user:secret@bucket/tables",
            "unknown://bucket/tables?query",
        ] {
            assert!(
                matches!(
                    Location::parse(text),
                    Err(EgtError::InvalidStorageLocation(_))
                ),
                "accepted unsupported URL components: {text}"
            );
        }
    }

    #[test]
    fn writability_does_not_depend_on_display() {
        let writable = Location {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: ObjectPath::from("tables"),
            display: "https://example.com/tables/".into(),
            writable: true,
            format_validated: Arc::new(OnceLock::new()),
        };
        writable.ensure_writable().unwrap();
        let read_only = Location {
            display: "memory:///tables/".into(),
            writable: false,
            ..writable
        };
        assert!(matches!(
            read_only.ensure_writable(),
            Err(EgtError::InvalidStorageLocation(_))
        ));
    }

    #[test]
    fn hierarchical_keys_preserve_segments() {
        let id = crate::table_id::TableId::parse("KPao_KPao").unwrap();
        for (url, prefix) in [
            ("memory:///nested/tables", "nested/tables/"),
            ("memory:///", ""),
        ] {
            let location = Location::parse(url).unwrap();
            for extension in ["ggegt", "json"] {
                let relative = id.relative_path(extension);
                let key = location.key(&relative);
                assert_eq!(
                    key.as_ref(),
                    format!("{prefix}4/2v2/op1/2pawns/KPao_KPao.{extension}")
                );
                assert_eq!(
                    location.describe(&key),
                    format!("{}/{}", location.display.trim_end_matches('/'), relative)
                );
                block_on(location.store.put(&key, b"nested artifact".to_vec().into())).unwrap();
                let expected = ObjectPath::from(format!("{prefix}{relative}"));
                let result = block_on(location.store.get(&expected)).unwrap();
                assert_eq!(
                    block_on(result.bytes()).unwrap().as_ref(),
                    b"nested artifact"
                );
            }
        }
    }

    #[test]
    fn prefixes_and_read_only_urls() {
        for text in ["memory:///tables", "memory:///tables/"] {
            let location = Location::parse(text).unwrap();
            assert_eq!(
                location.key("2/1v1/op0/0pawns/K_K.ggegt").as_ref(),
                "tables/2/1v1/op0/0pawns/K_K.ggegt"
            );
        }
        let http = Location::parse("https://example.com/tables/").unwrap();
        assert!(http.ensure_writable().is_err());
        assert!(Location::parse("unknown://bucket/tables").is_err());
    }
}
