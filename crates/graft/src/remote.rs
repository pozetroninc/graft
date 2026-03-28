use std::{future, ops::Range, path::PathBuf, time::Duration};

use crate::core::{LogId, SegmentId, cbe::CBE64, commit::Commit, lsn::LSN};
use bilrost::{Message, OwnedMessage};
use bytes::Bytes;
use futures::{
    Stream, StreamExt, TryStreamExt,
    stream::{self, FuturesOrdered},
};
use opendal::{
    ErrorKind, Operator,
    layers::{HttpClientLayer, RetryLayer},
    options::{ReadOptions, WriteOptions},
    raw::HttpClient,
    services::{Fs, Memory, S3},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod segment;

const REMOTE_CONCURRENCY: usize = 5;

enum RemotePath<'a> {
    /// Commits are stored at `/logs/{logid}/commits/{CBE64 hex LSN}`
    Commit(&'a LogId, LSN),

    /// Segments are stored at `/segments/{sid}`
    Segment(&'a SegmentId),
}

impl RemotePath<'_> {
    fn build(self) -> String {
        match self {
            Self::Commit(log, lsn) => format!(
                "logs/{}/commits/{}",
                &log.serialize(),
                &CBE64::from(lsn).to_string(),
            ),
            Self::Segment(sid) => format!("segments/{}", &sid.serialize()),
        }
    }
}

#[derive(Error, Debug)]
pub enum RemoteErr {
    #[error("Object store error: {0}")]
    ObjectStore(#[from] opendal::Error),

    #[error("HTTP client setup error: {0}")]
    SetupHttp(#[from] reqwest::Error),

    #[error("Failed to decode file: {0}")]
    Decode(#[from] bilrost::DecodeError),

    #[error("Certificate file error: {path}: {source}")]
    CertIo {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Invalid endpoint URL: {0}")]
    InvalidEndpoint(String),
}

impl RemoteErr {
    fn objectstore_err_kind(&self) -> Option<opendal::ErrorKind> {
        if let RemoteErr::ObjectStore(err) = self {
            Some(err.kind())
        } else {
            None
        }
    }

    pub fn precondition_failed(&self) -> bool {
        matches!(
            self.objectstore_err_kind(),
            Some(opendal::ErrorKind::ConditionNotMatch)
        )
    }

    pub fn is_not_found(&self) -> bool {
        matches!(
            self.objectstore_err_kind(),
            Some(opendal::ErrorKind::NotFound)
        )
    }
}

pub type Result<T> = std::result::Result<T, RemoteErr>;

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteConfig {
    /// In memory object store
    #[default]
    Memory,

    /// On disk object store
    Fs { root: String },

    /// S3 compatible object store
    /// Can load most config and secrets from standard AWS environment variables
    S3Compatible {
        bucket: String,
        prefix: Option<String>,
    },

    /// HTTP gateway backed by an S3-compatible store, authenticated via mTLS
    HttpGateway {
        endpoint: String,
        bucket: String,
        #[serde(default)]
        prefix: Option<String>,
        cert_dir: PathBuf,
    },
}

impl RemoteConfig {
    pub fn build(self) -> Result<Remote> {
        Remote::with_config(self)
    }
}

#[derive(Debug, Clone)]
pub struct Remote {
    store: Operator,
}

impl Remote {
    pub fn with_config(config: RemoteConfig) -> Result<Self> {
        let store = match config {
            RemoteConfig::Memory => Operator::new(Memory::default())?.finish(),
            RemoteConfig::Fs { root } => Operator::new(Fs::default().root(&root))?.finish(),
            RemoteConfig::S3Compatible { bucket, prefix } => {
                let mut builder = S3::default().bucket(&bucket);
                if let Some(prefix) = prefix {
                    builder = builder.root(&prefix);
                }
                if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
                    builder = builder.endpoint(&endpoint);
                }
                let client_builder = reqwest::ClientBuilder::new()
                    // use http1 to maximize throughput
                    // http2 routes all requests through a single connection
                    .http1_only()
                    // enable hickory DNS resolver for DNS caching
                    .hickory_dns(true)
                    .connect_timeout(Duration::from_secs(5));
                #[cfg(any(
                    target_os = "android",
                    target_os = "fuchsia",
                    target_os = "linux"
                ))]
                let client_builder = client_builder.tcp_user_timeout(Duration::from_secs(60));
                let client = client_builder.build()?;

                Operator::new(builder)?
                    .layer(HttpClientLayer::new(HttpClient::with(client)))
                    .layer(RetryLayer::new())
                    .finish()
            }
            RemoteConfig::HttpGateway {
                endpoint,
                bucket,
                prefix,
                cert_dir,
            } => {
                // Validate that the endpoint uses HTTPS (allow HTTP only for
                // localhost/127.0.0.1 for testing purposes)
                let parsed = reqwest::Url::parse(&endpoint).map_err(|_| {
                    RemoteErr::InvalidEndpoint(format!("failed to parse URL: {endpoint}"))
                })?;
                let is_localhost = matches!(
                    parsed.host_str(),
                    Some("localhost") | Some("127.0.0.1")
                );
                if parsed.scheme() != "https" && !is_localhost {
                    return Err(RemoteErr::InvalidEndpoint(format!(
                        "HttpGateway endpoint must use HTTPS: {endpoint}"
                    )));
                }

                let mut builder = S3::default()
                    .bucket(&bucket)
                    .endpoint(&endpoint)
                    .disable_config_load()
                    .disable_ec2_metadata()
                    .allow_anonymous();

                if let Some(prefix) = prefix {
                    builder = builder.root(&prefix);
                }

                // Load mTLS certificates from cert_dir
                let ca_path = cert_dir.join("ca.pem");
                let cert_path = cert_dir.join("cert.pem");
                let key_path = cert_dir.join("key.pem");

                let ca_pem = std::fs::read(&ca_path).map_err(|e| RemoteErr::CertIo {
                    path: ca_path,
                    source: e,
                })?;
                let cert_pem =
                    std::fs::read(&cert_path).map_err(|e| RemoteErr::CertIo {
                        path: cert_path,
                        source: e,
                    })?;
                let key_pem = std::fs::read(&key_path).map_err(|e| RemoteErr::CertIo {
                    path: key_path,
                    source: e,
                })?;

                let ca = reqwest::tls::Certificate::from_pem(&ca_pem)?;

                let mut identity_pem = cert_pem;
                identity_pem.extend_from_slice(&key_pem);
                let identity = reqwest::Identity::from_pem(&identity_pem)?;

                let client_builder = reqwest::ClientBuilder::new()
                    .http1_only()
                    .hickory_dns(true)
                    .connect_timeout(Duration::from_secs(5));
                #[cfg(any(
                    target_os = "android",
                    target_os = "fuchsia",
                    target_os = "linux"
                ))]
                let client_builder = client_builder.tcp_user_timeout(Duration::from_secs(60));
                let client = client_builder
                    .add_root_certificate(ca)
                    .identity(identity)
                    .build()?;

                Operator::new(builder)?
                    .layer(HttpClientLayer::new(HttpClient::with(client)))
                    .layer(RetryLayer::new())
                    .finish()
            }
        };

        Ok(Self { store })
    }

    /// Streams commits by LSN in the same order as the input iterator.
    /// Stops fetching commits as soon as we receive a `NotFound` error from the
    /// remote, thus even if `lsns` contains every LSN we will stop loading
    /// commits as soon as we reach the end of the log.
    pub fn stream_commits_ordered<I: IntoIterator<Item = LSN>>(
        &self,
        log: &LogId,
        lsns: I,
    ) -> impl Stream<Item = Result<Commit>> {
        // convert the set into a stream of chunks, such that the first chunk
        // only contains the first LSN, and the remaining chunks have a maximum
        // size of REPLAY_CONCURRENCY
        let mut lsns = lsns.into_iter();
        let first_chunk: Vec<LSN> = match lsns.next() {
            Some(lsn) => vec![lsn],
            None => vec![],
        };
        stream::once(future::ready(first_chunk))
            .chain(stream::iter(lsns).chunks(REMOTE_CONCURRENCY))
            .flat_map(|chunk| {
                chunk
                    .into_iter()
                    .map(|lsn| self.get_commit(log, lsn))
                    .collect::<FuturesOrdered<_>>()
            })
            .try_take_while(|result| future::ready(Ok(result.is_some())))
            .map_ok(|result| result.unwrap())
    }

    /// Fetches a single commit, returning None if the commit is not found.
    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self))]
    pub async fn get_commit(&self, log: &LogId, lsn: LSN) -> Result<Option<Commit>> {
        let path = RemotePath::Commit(log, lsn).build();
        match self.store.read(&path).await {
            Ok(res) => Ok(Some(Commit::decode(res)?)),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Atomically write a commit to the remote, returning
    /// `RemoteErr::ObjectStore(Error::AlreadyExists)` on a collision
    #[tracing::instrument(level = "debug", err(level = "debug"), skip(self, commit),
        fields(log = %commit.log, lsn = %commit.lsn, sid = ?commit.segment_id())
    )]
    pub async fn put_commit(&self, commit: &Commit) -> Result<()> {
        let path = RemotePath::Commit(commit.log(), commit.lsn()).build();
        self.store
            .write_options(
                &path,
                commit.encode_to_bytes(),
                WriteOptions {
                    // Perform an atomic write operation, returning
                    // a precondition error if the commit already exists
                    if_not_exists: true,
                    concurrent: REMOTE_CONCURRENCY,
                    ..WriteOptions::default()
                },
            )
            .await?;
        Ok(())
    }

    /// Uploads a segment to this Remote
    #[tracing::instrument(
        level = "debug",
        err(level = "debug"),
        skip(self, chunks),
        fields(size)
    )]
    pub async fn put_segment<I: IntoIterator<Item = Bytes>>(
        &self,
        sid: &SegmentId,
        chunks: I,
    ) -> Result<()> {
        let path = RemotePath::Segment(sid).build();
        let mut w = self
            .store
            .writer_with(&path)
            .concurrent(REMOTE_CONCURRENCY)
            .await?;
        let mut size = 0;
        for chunk in chunks {
            size += chunk.len();
            w.write(chunk).await?;
        }
        tracing::Span::current().record("size", size);
        w.close().await?;
        Ok(())
    }

    /// Reads a byte range of a segment
    #[tracing::instrument(level = "debug", err(level = "debug"), skip(self))]
    pub async fn get_segment_range(&self, sid: &SegmentId, bytes: Range<u64>) -> Result<Bytes> {
        let path = RemotePath::Segment(sid).build();
        let buffer = self
            .store
            .read_options(
                &path,
                ReadOptions {
                    range: bytes.into(),
                    concurrent: REMOTE_CONCURRENCY,
                    ..ReadOptions::default()
                },
            )
            .await?;
        Ok(buffer.to_bytes())
    }

    /// TESTONLY: list contents of this remote in a tree-like format
    #[cfg(test)]
    pub async fn testonly_format_tree(&self) -> String {
        use itertools::Itertools;
        use std::collections::BTreeMap;
        use text_trees::{
            AnchorPosition, FormatCharacters, TreeFormatting, TreeNode, TreeOrientation,
        };

        let paths = self
            .store
            .list("")
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.path().split("/").map(|s| s.to_string()).collect_vec())
            .collect_vec();

        #[derive(Default)]
        struct TreeBuilder {
            children: BTreeMap<String, TreeBuilder>,
        }

        impl TreeBuilder {
            fn insert(&mut self, parts: &[String]) {
                if parts.is_empty() {
                    return;
                }

                let first = &parts[0];
                let rest = &parts[1..];

                self.children.entry(first.clone()).or_default().insert(rest);
            }

            fn to_tree_node(self, name: String) -> TreeNode<String> {
                if self.children.is_empty() {
                    // This is a leaf node
                    TreeNode::new(name)
                } else {
                    // This is a directory node
                    let child_nodes = self
                        .children
                        .into_iter()
                        .map(|(name, builder)| builder.to_tree_node(name));
                    TreeNode::with_child_nodes(name, child_nodes)
                }
            }
        }

        let mut root = TreeBuilder::default();
        for path in paths {
            root.insert(&path);
        }

        root.to_tree_node(format!("{:?}", self.store))
            .to_string_with_format(&TreeFormatting {
                prefix_str: None,
                orientation: TreeOrientation::TopDown,
                anchor: AnchorPosition::Left,
                chars: FormatCharacters::box_chars(),
            })
            .unwrap()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_http_gateway_config_deserialize() {
        let toml_str = r#"
            type = "http_gateway"
            endpoint = "https://proxy.example.com:8443"
            bucket = "my-bucket"
            prefix = "users/alice"
            cert_dir = "/etc/certs"
        "#;

        let config: RemoteConfig = toml::from_str(toml_str).unwrap();
        match config {
            RemoteConfig::HttpGateway {
                endpoint,
                bucket,
                prefix,
                cert_dir,
            } => {
                assert_eq!(endpoint, "https://proxy.example.com:8443");
                assert_eq!(bucket, "my-bucket");
                assert_eq!(prefix.as_deref(), Some("users/alice"));
                assert_eq!(cert_dir, PathBuf::from("/etc/certs"));
            }
            other => panic!("expected HttpGateway, got {other:?}"),
        }
    }

    #[test]
    fn test_http_gateway_config_optional_prefix() {
        let toml_str = r#"
            type = "http_gateway"
            endpoint = "https://proxy.example.com:8443"
            bucket = "my-bucket"
            cert_dir = "/etc/certs"
        "#;

        let config: RemoteConfig = toml::from_str(toml_str).unwrap();
        match config {
            RemoteConfig::HttpGateway { prefix, .. } => {
                assert!(prefix.is_none());
            }
            other => panic!("expected HttpGateway, got {other:?}"),
        }
    }

    #[test]
    fn test_http_gateway_mtls_cert_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let cert_dir = tmp.path().to_path_buf();

        // Write dummy PEM files (syntactically valid PEM but not real certs).
        // We verify the file-reading stage succeeds (no CertIo error).
        let dummy_cert_pem =
            b"-----BEGIN CERTIFICATE-----\nMIIBkTCB+wIUZe0n/0WwDQ==\n-----END CERTIFICATE-----\n";
        let dummy_key_pem =
            b"-----BEGIN PRIVATE KEY-----\nMIIBkTCB+wIUZe0n/0WwDQ==\n-----END PRIVATE KEY-----\n";

        std::fs::write(cert_dir.join("ca.pem"), dummy_cert_pem).unwrap();
        std::fs::write(cert_dir.join("cert.pem"), dummy_cert_pem).unwrap();
        std::fs::write(cert_dir.join("key.pem"), dummy_key_pem).unwrap();

        let config = RemoteConfig::HttpGateway {
            endpoint: "https://proxy.example.com:8443".into(),
            bucket: "my-bucket".into(),
            prefix: None,
            cert_dir,
        };

        // The dummy PEM data is not a real certificate, so reqwest will
        // fail during TLS setup. The important thing is that we do NOT
        // get a CertIo error -- the files were read successfully.
        let err = Remote::with_config(config).unwrap_err();
        assert!(
            !matches!(err, RemoteErr::CertIo { .. }),
            "expected TLS setup error, not CertIo; got: {err}"
        );
    }

    #[test]
    fn test_http_gateway_missing_cert_returns_certio_with_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cert_dir = tmp.path().to_path_buf();

        // Don't create any PEM files -- ca.pem is missing.
        let config = RemoteConfig::HttpGateway {
            endpoint: "https://proxy.example.com:8443".into(),
            bucket: "my-bucket".into(),
            prefix: None,
            cert_dir: cert_dir.clone(),
        };

        let err = Remote::with_config(config).unwrap_err();
        match &err {
            RemoteErr::CertIo { path, source } => {
                assert_eq!(path, &cert_dir.join("ca.pem"));
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected CertIo, got {other:?}"),
        }
    }

    #[test]
    fn test_http_gateway_rejects_non_https_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let cert_dir = tmp.path().to_path_buf();

        let config = RemoteConfig::HttpGateway {
            endpoint: "http://insecure.example.com".into(),
            bucket: "my-bucket".into(),
            prefix: None,
            cert_dir,
        };

        let err = Remote::with_config(config).unwrap_err();
        assert!(
            matches!(err, RemoteErr::InvalidEndpoint(_)),
            "expected InvalidEndpoint, got {err:?}"
        );
    }

    #[test]
    fn test_http_gateway_allows_http_localhost() {
        // http://localhost should be allowed for testing, though it will
        // fail later because no cert files exist.
        let tmp = tempfile::tempdir().unwrap();
        let cert_dir = tmp.path().to_path_buf();

        let config = RemoteConfig::HttpGateway {
            endpoint: "http://localhost:8080".into(),
            bucket: "my-bucket".into(),
            prefix: None,
            cert_dir,
        };

        let err = Remote::with_config(config).unwrap_err();
        // Should fail with CertIo (missing files), NOT InvalidEndpoint
        assert!(
            matches!(err, RemoteErr::CertIo { .. }),
            "expected CertIo for localhost, got {err:?}"
        );
    }
}
