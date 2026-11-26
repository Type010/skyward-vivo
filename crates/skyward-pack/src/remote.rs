//! A ZIP archive on a server, read through HTTP range requests: a block
//! OTA package is several gigabytes, and only its recovery image is
//! wanted.

use anyhow::{bail, Context, Result};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

const BLOCK: u64 = 1 << 20;

/// The bytes of a remote file, a block at a time.
struct Remote {
    agent: ureq::Agent,
    url: String,
    size: u64,
    position: u64,
    block: Option<(u64, Arc<Vec<u8>>)>,
}

impl Remote {
    fn open(url: &str) -> Result<Remote> {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_recv_body(Some(std::time::Duration::from_secs(600))).build().into();
        let (data, size) = range(&agent, url, 0, 1).with_context(|| format!("read the start of {url}"))?;
        Ok(Remote { agent, url: url.to_owned(), size, position: 0, block: Some((0, Arc::new(data))) })
    }

    fn block(&mut self, index: u64) -> io::Result<Arc<Vec<u8>>> {
        if let Some((held, data)) = &self.block
            && *held == index
        {
            return Ok(data.clone());
        }
        let start = index * BLOCK;
        let end = (start + BLOCK).min(self.size);
        let (data, _) = range(&self.agent, &self.url, start, end).map_err(io::Error::other)?;
        let data = Arc::new(data);
        self.block = Some((index, data.clone()));
        Ok(data)
    }
}

impl Read for Remote {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.position >= self.size {
            return Ok(0);
        }
        let index = self.position / BLOCK;
        let data = self.block(index)?;
        let within = (self.position % BLOCK) as usize;
        let n = (data.len() - within).min(out.len());
        out[..n].copy_from_slice(&data[within..within + n]);
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for Remote {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let position = match to {
            SeekFrom::Start(at) => at as i128,
            SeekFrom::End(delta) => self.size as i128 + delta as i128,
            SeekFrom::Current(delta) => self.position as i128 + delta as i128,
        };
        if !(0..=self.size as i128).contains(&position) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek outside the archive"));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

/// Fetches bytes `start..end` of `url`, with the file's size.
fn range(agent: &ureq::Agent, url: &str, start: u64, end: u64) -> Result<(Vec<u8>, u64)> {
    let mut response = agent
        .get(url)
        .header("Range", format!("bytes={start}-{}", end - 1))
        .call()
        .with_context(|| format!("ask for bytes {start}-{} of {url}", end - 1))?;
    if response.status() != ureq::http::StatusCode::PARTIAL_CONTENT {
        bail!("{url} answered {} to a range request", response.status());
    }
    let total = response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit_once('/'))
        .and_then(|(_, total)| total.parse().ok())
        .with_context(|| format!("{url} names no size in its Content-Range"))?;
    let mut data = vec![0u8; (end - start) as usize];
    response.body_mut().as_reader().read_exact(&mut data).with_context(|| format!("read bytes {start}-{} of {url}", end - 1))?;
    Ok((data, total))
}

/// Takes `recovery` out of the ZIP archive at `url`, decompressing it.
pub fn recovery(url: &str, recovery: &str) -> Result<Vec<u8>> {
    let remote = Remote::open(url)?;
    let mut archive = zip::ZipArchive::new(remote).with_context(|| format!("read the ZIP archive at {url}"))?;
    let name = [recovery.to_owned(), format!("firmware-update/{recovery}.img"), format!("firmware-update/{recovery}")]
        .into_iter()
        .find(|name| archive.by_name(name).is_ok())
        .with_context(|| format!("{url} holds no {recovery} image"))?;
    let mut entry = archive.by_name(&name).with_context(|| format!("{url} holds no {name}"))?;
    let size = usize::try_from(entry.size()).context("the recovery image is too large")?;
    let mut data = Vec::with_capacity(size);
    entry.read_to_end(&mut data).with_context(|| format!("read {name} out of {url}"))?;
    Ok(data)
}
