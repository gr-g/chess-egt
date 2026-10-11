//! Location parsing and the async boundary for synchronous tablebase operations.

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use object_store::{ObjectStore, ObjectStoreScheme, path::Path as ObjectPath};
use tokio::runtime::Runtime;
use url::Url;

use crate::{EgtError, EgtResult};

/// A shared backend and a base prefix.
#[derive(Debug, Clone)]
pub(crate) struct Location {
    pub(crate) store: Arc<dyn ObjectStore>,
    pub(crate) prefix: ObjectPath,
    pub(crate) display: String,
    pub(crate) writable: bool,
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
        })
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
    fn prefixes_and_read_only_urls() {
        for text in ["memory:///tables", "memory:///tables/"] {
            let location = Location::parse(text).unwrap();
            assert_eq!(
                location.prefix.clone().join("K_K.ggegt").as_ref(),
                "tables/K_K.ggegt"
            );
        }
        let http = Location::parse("https://example.com/tables/").unwrap();
        assert!(http.ensure_writable().is_err());
        assert!(Location::parse("unknown://bucket/tables").is_err());
    }
}
