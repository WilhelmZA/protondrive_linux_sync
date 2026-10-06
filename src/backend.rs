//! Backend construction shared by all sync entry points.

use anyhow::{bail, Result};

use crate::config::{Backend, Config};
use crate::driveapi::DriveApi;
use crate::logger::Logger;
use crate::models::{DownloadJob, Entry, TreeScan};
use crate::protoncli::{ListOutcome, ProtonCli, Remote};
use std::sync::atomic::AtomicBool;

pub fn select(cfg: &Config) -> Result<Box<dyn Remote + Send + Sync>> {
    match cfg.backend {
        Backend::Api => Ok(Box::new(DriveApi::new(cfg)?)),
        Backend::Cli => {
            let remote = ProtonCli::new(cfg);
            if remote.resolve_binary().is_none() {
                bail!(
                    "proton-drive not found (configured: {:?}). Install it or set cli.binary.",
                    cfg.binary
                );
            }
            Ok(Box::new(remote))
        }
    }
}

pub fn effective_dry_run(remote: &dyn Remote, requested: bool, log: &Logger) -> bool {
    if remote.read_only() && !requested {
        log.warn("read-only backend: forcing a dry run, nothing will change");
    }
    requested || remote.read_only()
}

// Forward every method, including the CLI's optimised tree scan and transfers.
impl<R: Remote + ?Sized> Remote for Box<R> {
    fn read_only(&self) -> bool {
        (**self).read_only()
    }
    fn list_dir(&self, path: &str) -> Result<Vec<Entry>> {
        (**self).list_dir(path)
    }
    fn list_dir_probe(&self, path: &str) -> Result<ListOutcome> {
        (**self).list_dir_probe(path)
    }
    fn create_folder(&self, parent: &str, name: &str) -> Result<()> {
        (**self).create_folder(parent, name)
    }
    fn upload(&self, local: &str, parent: &str) -> Result<()> {
        (**self).upload(local, parent)
    }
    fn download(&self, remote: &str, dest: &str) -> Result<()> {
        (**self).download(remote, dest)
    }
    fn trash(&self, path: &str) -> Result<()> {
        (**self).trash(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        (**self).rename(from, to)
    }
    fn list_tree(
        &self,
        base: &str,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<TreeScan> {
        (**self).list_tree(base, exclude, progress)
    }
    fn download_many(
        &self,
        jobs: &[DownloadJob],
        threads: usize,
        cancel: &AtomicBool,
        on_start: &(dyn Fn(&DownloadJob) + Sync),
        on_done: &(dyn Fn(&DownloadJob, std::result::Result<(), String>) + Sync),
    ) {
        (**self).download_many(jobs, threads, cancel, on_start, on_done)
    }
}
