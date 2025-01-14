#![feature(async_closure)]
#![feature(let_chains)]
#![feature(io_error_more)]
#![feature(if_let_guard)]
mod azure_blob;
mod google_cloud;
mod http;
mod huggingface;
mod local;
mod object_io;
mod object_store_glob;
mod s3_like;
mod stats;
mod stream_utils;
use azure_blob::AzureBlobSource;
use futures::FutureExt;
use google_cloud::GCSSource;
use huggingface::HFSource;
use lazy_static::lazy_static;
mod file_format;
#[cfg(feature = "python")]
pub mod python;
pub use file_format::FileFormat;

pub use common_io_config::{AzureConfig, IOConfig, S3Config};
pub use object_io::FileMetadata;
pub use object_io::GetResult;
use object_io::StreamingRetryParams;
#[cfg(feature = "python")]
pub use python::register_modules;
pub use stats::{IOStatsContext, IOStatsRef};
use tokio::runtime::RuntimeFlavor;
use tokio::task::JoinHandle;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::OnceLock;
use std::{borrow::Cow, collections::HashMap, hash::Hash, ops::Range};

use futures::stream::BoxStream;

use snafu::Snafu;
use url::ParseError;

use snafu::prelude::*;

use common_error::{DaftError, DaftResult};
use s3_like::S3LikeSource;

use self::{http::HttpSource, local::LocalSource, object_io::ObjectSource};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Generic {} error: {}", store, source))]
    Generic { store: SourceType, source: DynError },
    #[snafu(display("Object at location {} not found\nDetails:\n{}", path, source))]
    NotFound { path: String, source: DynError },

    #[snafu(display("Invalid Argument: {:?}", msg))]
    InvalidArgument { msg: String },

    #[snafu(display("Unable to expand home dir"))]
    HomeDirError { path: String },

    #[snafu(display("Unable to open file {}: {:?}", path, source))]
    UnableToOpenFile { path: String, source: DynError },

    #[snafu(display("Unable to create directory {}: {:?}", path, source))]
    UnableToCreateDir {
        path: String,
        source: std::io::Error,
    },

    #[snafu(display("Unable to read data from file {}: {}", path, source))]
    UnableToReadBytes {
        path: String,
        source: std::io::Error,
    },

    #[snafu(display("Unable to write data to file {}: {}", path, source))]
    UnableToWriteToFile {
        path: String,
        source: std::io::Error,
    },

    #[snafu(display(
        "Connection timed out when trying to connect to {}\nDetails:\n{:?}",
        path,
        source
    ))]
    ConnectTimeout { path: String, source: DynError },

    #[snafu(display("Read timed out when trying to read {}\nDetails:\n{:?}", path, source))]
    ReadTimeout { path: String, source: DynError },

    #[snafu(display(
        "Socket error occurred when trying to read {}\nDetails:\n{:?}",
        path,
        source
    ))]
    SocketError { path: String, source: DynError },

    #[snafu(display("Unable to convert URL \"{}\" to path", path))]
    InvalidUrl {
        path: String,
        source: url::ParseError,
    },

    #[snafu(display("Not a File: \"{}\"", path))]
    NotAFile { path: String },

    #[snafu(display("Unable to determine size of {}", path))]
    UnableToDetermineSize { path: String },

    #[snafu(display("Unable to load Credentials for store: {store}\nDetails:\n{source:?}"))]
    UnableToLoadCredentials { store: SourceType, source: DynError },

    #[snafu(display("Failed to load Credentials for store: {store}\nDetails:\n{source:?}"))]
    UnableToCreateClient { store: SourceType, source: DynError },

    #[snafu(display("Unauthorized to access store: {store} for file: {path}\nYou may need to set valid Credentials\n{source}"))]
    Unauthorized {
        store: SourceType,
        path: String,
        source: DynError,
    },

    #[snafu(display("Source not yet implemented: {}", store))]
    NotImplementedSource { store: String },

    #[snafu(display("Unhandled Error for path: {}\nDetails:\n{}", path, msg))]
    Unhandled { path: String, msg: String },

    #[snafu(
        display("Error sending data over a tokio channel: {}", source),
        context(false)
    )]
    UnableToSendDataOverChannel { source: DynError },

    #[snafu(display("Error joining spawned task: {}", source), context(false))]
    JoinError { source: tokio::task::JoinError },

    #[snafu(display("Cached error: {}", source))]
    CachedError { source: Arc<Error> },
}

impl From<Error> for DaftError {
    fn from(err: Error) -> DaftError {
        use Error::*;
        match err {
            NotFound { path, source } => DaftError::FileNotFound { path, source },
            ConnectTimeout { .. } => DaftError::ConnectTimeout(err.into()),
            ReadTimeout { .. } => DaftError::ReadTimeout(err.into()),
            UnableToReadBytes { .. } => DaftError::ByteStreamError(err.into()),
            SocketError { .. } => DaftError::SocketError(err.into()),
            // We have to repeat everything above for the case we have an Arc since we can't move the error.
            CachedError { ref source } => match source.as_ref() {
                NotFound { path, source: _ } => DaftError::FileNotFound {
                    path: path.clone(),
                    source: err.into(),
                },
                ConnectTimeout { .. } => DaftError::ConnectTimeout(err.into()),
                ReadTimeout { .. } => DaftError::ReadTimeout(err.into()),
                UnableToReadBytes { .. } => DaftError::ByteStreamError(err.into()),
                SocketError { .. } => DaftError::SocketError(err.into()),
                _ => DaftError::External(err.into()),
            },
            _ => DaftError::External(err.into()),
        }
    }
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Other, err)
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Default)]
pub struct IOClient {
    source_type_to_store: tokio::sync::RwLock<HashMap<SourceType, Arc<dyn ObjectSource>>>,
    config: Arc<IOConfig>,
}

impl IOClient {
    pub fn new(config: Arc<IOConfig>) -> Result<Self> {
        Ok(IOClient {
            source_type_to_store: tokio::sync::RwLock::new(HashMap::new()),
            config,
        })
    }

    async fn get_source(&self, source_type: &SourceType) -> Result<Arc<dyn ObjectSource>> {
        {
            if let Some(client) = self.source_type_to_store.read().await.get(source_type) {
                return Ok(client.clone());
            }
        }
        let mut w_handle = self.source_type_to_store.write().await;

        if let Some(client) = w_handle.get(source_type) {
            return Ok(client.clone());
        }

        let new_source = match source_type {
            SourceType::File => LocalSource::get_client().await? as Arc<dyn ObjectSource>,
            SourceType::Http => {
                HttpSource::get_client(&self.config.http).await? as Arc<dyn ObjectSource>
            }
            SourceType::S3 => {
                S3LikeSource::get_client(&self.config.s3).await? as Arc<dyn ObjectSource>
            }
            SourceType::AzureBlob => {
                AzureBlobSource::get_client(&self.config.azure).await? as Arc<dyn ObjectSource>
            }

            SourceType::GCS => {
                GCSSource::get_client(&self.config.gcs).await? as Arc<dyn ObjectSource>
            }
            SourceType::HF => {
                HFSource::get_client(&self.config.http).await? as Arc<dyn ObjectSource>
            }
        };

        if w_handle.get(source_type).is_none() {
            w_handle.insert(*source_type, new_source.clone());
        }
        Ok(new_source)
    }

    pub async fn glob(
        &self,
        input: String,
        fanout_limit: Option<usize>,
        page_size: Option<i32>,
        limit: Option<usize>,
        io_stats: Option<Arc<IOStatsContext>>,
        file_format: Option<FileFormat>,
    ) -> Result<BoxStream<'static, Result<FileMetadata>>> {
        let (scheme, _) = parse_url(input.as_str())?;
        let source = self.get_source(&scheme).await?;
        let files = source
            .glob(
                input.as_str(),
                fanout_limit,
                page_size,
                limit,
                io_stats,
                file_format,
            )
            .await?;
        Ok(files)
    }

    pub async fn single_url_get(
        &self,
        input: String,
        range: Option<Range<usize>>,
        io_stats: Option<IOStatsRef>,
    ) -> Result<GetResult> {
        let (scheme, path) = parse_url(&input)?;
        let source = self.get_source(&scheme).await?;
        let get_result = source
            .get(path.as_ref(), range.clone(), io_stats.clone())
            .await?;
        Ok(get_result.with_retry(StreamingRetryParams::new(source, input, range, io_stats)))
    }

    pub async fn single_url_put(
        &self,
        dest: &str,
        data: bytes::Bytes,
        io_stats: Option<IOStatsRef>,
    ) -> Result<()> {
        let (scheme, dest) = parse_url(dest)?;
        let source = self.get_source(&scheme).await?;
        source.put(dest.as_ref(), data, io_stats.clone()).await
    }

    pub async fn single_url_get_size(
        &self,
        input: String,
        io_stats: Option<IOStatsRef>,
    ) -> Result<usize> {
        let (scheme, path) = parse_url(&input)?;
        let source = self.get_source(&scheme).await?;
        source.get_size(path.as_ref(), io_stats).await
    }

    pub async fn single_url_download(
        &self,
        index: usize,
        input: Option<String>,
        raise_error_on_failure: bool,
        io_stats: Option<IOStatsRef>,
    ) -> Result<Option<bytes::Bytes>> {
        let value = if let Some(input) = input {
            let response = self.single_url_get(input, None, io_stats).await;
            let res = match response {
                Ok(res) => res.bytes().await,
                Err(err) => Err(err),
            };
            Some(res)
        } else {
            None
        };

        match value {
            Some(Ok(bytes)) => Ok(Some(bytes)),
            Some(Err(err)) => match raise_error_on_failure {
                true => Err(err),
                false => {
                    log::warn!(
                        "Error occurred during url_download at index: {index} {} (falling back to Null)",
                        err
                    );
                    Ok(None)
                }
            },
            None => Ok(None),
        }
    }

    pub async fn single_url_upload(
        &self,
        index: usize,
        dest: String,
        data: Option<bytes::Bytes>,
        io_stats: Option<IOStatsRef>,
    ) -> Result<Option<String>> {
        let value = if let Some(data) = data {
            let response = self.single_url_put(dest.as_str(), data, io_stats).await;
            Some(response)
        } else {
            None
        };

        match value {
            Some(Ok(())) => Ok(Some(dest)),
            Some(Err(err)) => {
                log::warn!(
                    "Error occurred during file upload at index: {index} {} (falling back to Null)",
                    err
                );
                Err(err)
            }
            None => Ok(None),
        }
    }
}

#[derive(Debug, Hash, PartialEq, std::cmp::Eq, Clone, Copy)]
pub enum SourceType {
    File,
    Http,
    S3,
    AzureBlob,
    GCS,
    HF,
}

impl std::fmt::Display for SourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SourceType::File => write!(f, "file"),
            SourceType::Http => write!(f, "http"),
            SourceType::S3 => write!(f, "s3"),
            SourceType::AzureBlob => write!(f, "AzureBlob"),
            SourceType::GCS => write!(f, "gcs"),
            SourceType::HF => write!(f, "hf"),
        }
    }
}

pub fn parse_url(input: &str) -> Result<(SourceType, Cow<'_, str>)> {
    let mut fixed_input = Cow::Borrowed(input);
    // handle tilde `~` expansion
    if input.starts_with("~/") {
        return home::home_dir()
            .and_then(|home_dir| {
                let expanded = home_dir.join(&input[2..]);
                let input = expanded.to_str()?;

                Some((SourceType::File, Cow::Owned(format!("file://{}", input))))
            })
            .ok_or_else(|| crate::Error::InvalidArgument {
                msg: "Could not convert expanded path to string".to_string(),
            });
    }

    let url = match url::Url::parse(input) {
        Ok(url) => Ok(url),
        Err(ParseError::RelativeUrlWithoutBase) => {
            fixed_input = Cow::Owned(format!("file://{input}"));

            url::Url::parse(fixed_input.as_ref())
        }
        Err(err) => Err(err),
    }
    .context(InvalidUrlSnafu { path: input })?;

    let scheme = url.scheme().to_lowercase();
    match scheme.as_ref() {
        "file" => Ok((SourceType::File, fixed_input)),
        "http" | "https" => Ok((SourceType::Http, fixed_input)),
        "s3" | "s3a" => Ok((SourceType::S3, fixed_input)),
        "az" | "abfs" | "abfss" => Ok((SourceType::AzureBlob, fixed_input)),
        "gcs" | "gs" => Ok((SourceType::GCS, fixed_input)),
        "hf" => Ok((SourceType::HF, fixed_input)),
        #[cfg(target_env = "msvc")]
        _ if scheme.len() == 1 && ("a" <= scheme.as_str() && (scheme.as_str() <= "z")) => {
            Ok((SourceType::File, Cow::Owned(format!("file://{input}"))))
        }
        _ => Err(Error::NotImplementedSource { store: scheme }),
    }
}
type CacheKey = (bool, Arc<IOConfig>);

static THREADED_RUNTIME: OnceLock<RuntimeRef> = OnceLock::new();
static SINGLE_THREADED_RUNTIME: OnceLock<RuntimeRef> = OnceLock::new();

lazy_static! {
    static ref NUM_CPUS: usize = std::thread::available_parallelism().unwrap().get();
    static ref THREADED_RUNTIME_NUM_WORKER_THREADS: usize = 8.min(*NUM_CPUS);
    static ref CLIENT_CACHE: std::sync::RwLock<HashMap<CacheKey, Arc<IOClient>>> =
        std::sync::RwLock::new(HashMap::new());
}

pub fn get_io_client(multi_thread: bool, config: Arc<IOConfig>) -> DaftResult<Arc<IOClient>> {
    let read_handle = CLIENT_CACHE.read().unwrap();
    let key = (multi_thread, config.clone());
    if let Some(client) = read_handle.get(&key) {
        Ok(client.clone())
    } else {
        drop(read_handle);

        let mut w_handle = CLIENT_CACHE.write().unwrap();
        if let Some(client) = w_handle.get(&key) {
            Ok(client.clone())
        } else {
            let client = Arc::new(IOClient::new(config.clone())?);
            w_handle.insert(key, client.clone());
            Ok(client)
        }
    }
}

pub type RuntimeRef = Arc<Runtime>;

pub struct Runtime {
    runtime: tokio::runtime::Runtime,
}

impl Runtime {
    fn new(runtime: tokio::runtime::Runtime) -> RuntimeRef {
        Arc::new(Self { runtime })
    }

    /// Similar to tokio's Runtime::block_on but requires static lifetime + Send
    /// You should use this when you are spawning IO tasks from an Expression Evaluator or in the Executor
    pub fn block_on_io_pool<F>(&self, future: F) -> DaftResult<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let _join_handle = self.spawn(async move {
            let task_output = AssertUnwindSafe(future).catch_unwind().await.map_err(|e| {
                let s = if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown internal error".to_string()
                };
                DaftError::ComputeError(format!(
                    "Caught panic when spawning blocking task in io pool {s})"
                ))
            });

            if tx.send(task_output).is_err() {
                log::warn!("Spawned task output ignored: receiver dropped")
            }
        });
        rx.recv().expect("Spawned task transmitter dropped")
    }

    /// Blocks current thread to compute future. Can not be called in tokio runtime context
    ///
    pub fn block_on_current_thread<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }
}

fn init_runtime(num_threads: usize) -> Arc<Runtime> {
    std::thread::spawn(move || {
        Runtime::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(num_threads)
                .enable_all()
                .build()
                .unwrap(),
        )
    })
    .join()
    .unwrap()
}

pub fn get_runtime(multi_thread: bool) -> DaftResult<RuntimeRef> {
    match multi_thread {
        false => {
            let runtime = SINGLE_THREADED_RUNTIME
                .get_or_init(|| init_runtime(1))
                .clone();
            Ok(runtime)
        }
        true => {
            let runtime = THREADED_RUNTIME
                .get_or_init(|| init_runtime(*THREADED_RUNTIME_NUM_WORKER_THREADS))
                .clone();
            Ok(runtime)
        }
    }
}

pub fn get_io_pool_num_threads() -> Option<usize> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            match handle.runtime_flavor() {
                RuntimeFlavor::CurrentThread => Some(1),
                RuntimeFlavor::MultiThread => Some(*THREADED_RUNTIME_NUM_WORKER_THREADS),
                // RuntimeFlavor is #non_exhaustive, so we default to 1 here to be conservative
                _ => Some(1),
            }
        }
        Err(_) => None,
    }
}

type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;
