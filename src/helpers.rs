use std::{
    fs,
    future::Future,
    io::{self, stderr, stdin, Write},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use cargo_toml::Manifest;
use flate2::read::GzDecoder;
use futures_util::stream::StreamExt;
use log::{debug, info};
use reqwest::Method;
use scopeguard::ScopeGuard;
use serde::Serialize;
use tar::Archive;
use tinytemplate::TinyTemplate;
use tokio::{sync::mpsc, task};
use url::Url;
use xz2::read::XzDecoder;
use zip::read::ZipArchive;
use zstd::stream::Decoder as ZstdDecoder;

use crate::{BinstallError, Meta, PkgFmt};

/// Load binstall metadata from the crate `Cargo.toml` at the provided path
pub fn load_manifest_path<P: AsRef<Path>>(
    manifest_path: P,
) -> Result<Manifest<Meta>, BinstallError> {
    debug!("Reading manifest: {}", manifest_path.as_ref().display());

    // Load and parse manifest (this checks file system for binary output names)
    let manifest = Manifest::<Meta>::from_path_with_metadata(manifest_path)?;

    // Return metadata
    Ok(manifest)
}

pub async fn remote_exists(url: Url, method: Method) -> Result<bool, BinstallError> {
    let req = reqwest::Client::new()
        .request(method.clone(), url.clone())
        .send()
        .await
        .map_err(|err| BinstallError::Http { method, url, err })?;
    Ok(req.status().is_success())
}

/// Download a file from the provided URL to the provided path
pub async fn download<P: AsRef<Path>>(url: &str, path: P) -> Result<(), BinstallError> {
    let url = Url::parse(url)?;
    debug!("Downloading from: '{url}'");

    let resp = reqwest::get(url.clone())
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|err| BinstallError::Http {
            method: Method::GET,
            url,
            err,
        })?;

    let path = path.as_ref();
    debug!("Downloading to file: '{}'", path.display());

    let mut bytes_stream = resp.bytes_stream();
    let mut writer = AsyncFileWriter::new(path)?;

    let guard = scopeguard::guard(path, |path| {
        fs::remove_file(path).ok();
    });

    while let Some(res) = bytes_stream.next().await {
        writer.write(res?).await?;
    }

    writer.done().await?;
    // Disarm as it is successfully downloaded and written to file.
    ScopeGuard::into_inner(guard);

    debug!("Download OK, written to file: '{}'", path.display());

    Ok(())
}

/// Extract files from the specified source onto the specified path
pub fn extract<S: AsRef<Path>, P: AsRef<Path>>(
    source: S,
    fmt: PkgFmt,
    path: P,
) -> Result<(), BinstallError> {
    let source = source.as_ref();
    let path = path.as_ref();

    match fmt {
        PkgFmt::Tar => {
            // Extract to install dir
            debug!("Extracting from tar archive '{source:?}' to `{path:?}`");

            let dat = fs::File::open(source)?;
            let mut tar = Archive::new(dat);

            tar.unpack(path)?;
        }
        PkgFmt::Tgz => {
            // Extract to install dir
            debug!("Decompressing from tgz archive '{source:?}' to `{path:?}`");

            let dat = fs::File::open(source)?;
            let tar = GzDecoder::new(dat);
            let mut tgz = Archive::new(tar);

            tgz.unpack(path)?;
        }
        PkgFmt::Txz => {
            // Extract to install dir
            debug!("Decompressing from txz archive '{source:?}' to `{path:?}`");

            let dat = fs::File::open(source)?;
            let tar = XzDecoder::new(dat);
            let mut txz = Archive::new(tar);

            txz.unpack(path)?;
        }
        PkgFmt::Tzstd => {
            // Extract to install dir
            debug!("Decompressing from tzstd archive '{source:?}' to `{path:?}`");

            let dat = std::fs::File::open(source)?;

            // The error can only come from raw::Decoder::with_dictionary
            // as of zstd 0.10.2 and 0.11.2, which is specified
            // as &[] by ZstdDecoder::new, thus ZstdDecoder::new
            // should not return any error.
            let tar = ZstdDecoder::new(dat)?;
            let mut txz = Archive::new(tar);

            txz.unpack(path)?;
        }
        PkgFmt::Zip => {
            // Extract to install dir
            debug!("Decompressing from zip archive '{source:?}' to `{path:?}`");

            let dat = fs::File::open(source)?;
            let mut zip = ZipArchive::new(dat)?;

            zip.extract(path)?;
        }
        PkgFmt::Bin => {
            debug!("Copying binary '{source:?}' to `{path:?}`");
            // Copy to install dir
            fs::copy(source, path)?;
        }
    };

    Ok(())
}

/// Fetch install path from environment
/// roughly follows <https://doc.rust-lang.org/cargo/commands/cargo-install.html#description>
pub fn get_install_path<P: AsRef<Path>>(install_path: Option<P>) -> Option<PathBuf> {
    // Command line override first first
    if let Some(p) = install_path {
        return Some(PathBuf::from(p.as_ref()));
    }

    // Environmental variables
    if let Ok(p) = std::env::var("CARGO_INSTALL_ROOT") {
        debug!("using CARGO_INSTALL_ROOT ({p})");
        let b = PathBuf::from(p);
        return Some(b.join("bin"));
    }
    if let Ok(p) = std::env::var("CARGO_HOME") {
        debug!("using CARGO_HOME ({p})");
        let b = PathBuf::from(p);
        return Some(b.join("bin"));
    }

    // Standard $HOME/.cargo/bin
    if let Some(d) = dirs::home_dir() {
        let d = d.join(".cargo/bin");
        if d.exists() {
            debug!("using $HOME/.cargo/bin");

            return Some(d);
        }
    }

    // Local executable dir if no cargo is found
    let dir = dirs::executable_dir();

    if let Some(d) = &dir {
        debug!("Fallback to {}", d.display());
    }

    dir
}

pub fn confirm() -> Result<(), BinstallError> {
    loop {
        info!("Do you wish to continue? yes/[no]");
        eprint!("? ");
        stderr().flush().ok();

        let mut input = String::new();
        stdin().read_line(&mut input).unwrap();

        match input.as_str().trim() {
            "yes" | "y" | "YES" | "Y" => break Ok(()),
            "no" | "n" | "NO" | "N" | "" => break Err(BinstallError::UserAbort),
            _ => continue,
        }
    }
}

pub trait Template: Serialize {
    fn render(&self, template: &str) -> Result<String, BinstallError>
    where
        Self: Sized,
    {
        // Create template instance
        let mut tt = TinyTemplate::new();

        // Add template to instance
        tt.add_template("path", template)?;

        // Render output
        Ok(tt.render("path", self)?)
    }
}

#[derive(Debug)]
pub struct AsyncFileWriter {
    /// Use AutoAbortJoinHandle so that the task
    /// will be cancelled on failure.
    handle: AutoAbortJoinHandle<io::Result<()>>,
    tx: mpsc::Sender<Bytes>,
}

impl AsyncFileWriter {
    pub fn new(path: &Path) -> io::Result<Self> {
        fs::create_dir_all(path.parent().unwrap())?;

        let mut file = fs::File::create(path)?;
        let (tx, mut rx) = mpsc::channel::<Bytes>(100);

        let handle = AutoAbortJoinHandle::new(task::spawn_blocking(move || {
            while let Some(bytes) = rx.blocking_recv() {
                file.write_all(&*bytes)?;
            }

            rx.close();
            file.flush()?;

            Ok(())
        }));

        Ok(Self { handle, tx })
    }

    /// Upon error, this writer shall not be reused.
    /// Otherwise, `Self::done` would panic.
    pub async fn write(&mut self, bytes: Bytes) -> io::Result<()> {
        let send_future = async {
            self.tx
                .send(bytes)
                .await
                .expect("Implementation bug: rx is closed before tx is dropped")
        };
        tokio::pin!(send_future);

        let task_future = async {
            Self::wait(&mut self.handle).await.map(|_| {
                panic!("Implementation bug: write task finished before all writes are done")
            })
        };
        tokio::pin!(task_future);

        // Use select to run them in parallel, so that if the send blocks
        // the current future and the task failed with some error, the future
        // returned by this function would not block forever.
        tokio::select! {
            // It isn't completely safe to cancel the send_future as it would
            // cause us to lose our place in the queue, but if the send_future
            // is cancelled, it means that the task has failed and the mpsc
            // won't matter anyway.
            _ = send_future => Ok(()),
            res = task_future => res,
        }
    }

    pub async fn done(mut self) -> io::Result<()> {
        // Drop tx as soon as possible so that the task would wrap up what it
        // was doing and flush out all the pending data.
        drop(self.tx);

        Self::wait(&mut self.handle).await
    }

    async fn wait(handle: &mut AutoAbortJoinHandle<io::Result<()>>) -> io::Result<()> {
        match handle.await {
            Ok(res) => res,
            Err(join_err) => Err(io::Error::new(io::ErrorKind::Other, join_err)),
        }
    }
}

#[derive(Debug)]
pub struct AutoAbortJoinHandle<T>(task::JoinHandle<T>);

impl<T> AutoAbortJoinHandle<T> {
    pub fn new(handle: task::JoinHandle<T>) -> Self {
        Self(handle)
    }
}

impl<T> Drop for AutoAbortJoinHandle<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Deref for AutoAbortJoinHandle<T> {
    type Target = task::JoinHandle<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for AutoAbortJoinHandle<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Future for AutoAbortJoinHandle<T> {
    type Output = Result<T, task::JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut Pin::into_inner(self).0).poll(cx)
    }
}
