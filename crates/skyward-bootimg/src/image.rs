use abootimg_oxide::{binrw::io::Cursor, Header, HeaderV0, HeaderV0Versioned};
use anyhow::{anyhow, Context, Result};
use sha1::{Digest, Sha1};
use std::fs;
use std::path::Path;

/// A boot image with header version 0 through 4 (`boot`, `init_boot`,
/// `recovery`) split into its header, kernel, ramdisk, and what follows the
/// ramdisk: the second bootloader and DTBs of a version 0 through 2 header.
pub struct BootImage {
    pub header: Header,
    pub kernel: Vec<u8>,
    pub ramdisk: Vec<u8>,
    pub extra: Vec<u8>,
}

fn section(bytes: &[u8], at: usize, len: u32, what: &str) -> Result<Vec<u8>> {
    let end = at + len as usize;
    bytes.get(at..end).map(<[u8]>::to_vec).ok_or_else(|| anyhow!("{what} runs past the end of the image"))
}

impl BootImage {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        BootImage::parse(&bytes).with_context(|| format!("read {}", path.display()))
    }

    /// The image `bytes` hold, as a partition holds it: the image, then
    /// whatever fills the partition after it.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let header = Header::parse(&mut Cursor::new(bytes)).context("header")?;
        let kernel = section(bytes, header.kernel_position(), header.kernel_size(), "kernel")?;
        let ramdisk = section(bytes, header.ramdisk_position(), header.ramdisk_size(), "ramdisk")?;
        // A version 2 and older image keeps the second bootloader, a
        // recovery DTBO and a DTB after the ramdisk; the recovery's own
        // image has them, and writing it back with the ramdisk changed must
        // keep them, with the header's DTBO offset following them.
        let extra = match &header {
            Header::V0(header) => {
                let start = header.second_bootloader_position();
                let end = header.boot_image_size();
                bytes.get(start..end).map(<[u8]>::to_vec).ok_or_else(|| anyhow!("the sections after the ramdisk run past the end of the image"))?
            }
            Header::V3(_) => Vec::new(),
        };
        Ok(BootImage { header, kernel, ramdisk, extra })
    }

    /// The image with sizes taken from its parts. A version 4 header keeps its
    /// version but carries no boot signature; unlocked bootloaders do not ask
    /// for one.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut header = self.header.clone();
        match &mut header {
            Header::V0(header) => {
                header.kernel_size = u32::try_from(self.kernel.len()).context("kernel size")?;
                header.ramdisk_size = u32::try_from(self.ramdisk.len()).context("ramdisk size")?;
                // The bootloader reads the recovery DTBO at the offset the
                // header names; a ramdisk of another size moves it, so the
                // header has to name where it now is.
                set_recovery_dtbo_offset(header);
                update_hash(header, &self.kernel, &self.ramdisk, &self.extra)?;
            }
            Header::V3(header) => {
                header.kernel_size = u32::try_from(self.kernel.len()).context("kernel size")?;
                header.ramdisk_size = u32::try_from(self.ramdisk.len()).context("ramdisk size")?;
                header.v4_signature_size = header.v4_signature_size.map(|_| 0);
            }
        }
        let page = header.page_size();
        let mut out = Vec::new();
        header.write(&mut out).context("write header")?;
        pad(&mut out, page);
        out.extend_from_slice(&self.kernel);
        pad(&mut out, page);
        out.extend_from_slice(&self.ramdisk);
        pad(&mut out, page);
        out.extend_from_slice(&self.extra);
        Ok(out)
    }
}

/// Points the recovery DTBO offset of a version 1 or 2 header at the section
/// after the ramdisk, where the header and its bootloader put it.
fn set_recovery_dtbo_offset(header: &mut HeaderV0) {
    let recovery_dtbo_size = match header.versioned {
        HeaderV0Versioned::V0 => return,
        HeaderV0Versioned::V1 { recovery_dtbo_size, .. } | HeaderV0Versioned::V2 { recovery_dtbo_size, .. } => recovery_dtbo_size,
    };
    if recovery_dtbo_size == 0 {
        return;
    }
    let page = u64::from(header.page_size);
    let offset = page
        + align(u64::from(header.kernel_size), page)
        + align(u64::from(header.ramdisk_size), page)
        + align(u64::from(header.second_bootloader_size), page);
    match &mut header.versioned {
        HeaderV0Versioned::V0 => {}
        HeaderV0Versioned::V1 { recovery_dtbo_addr, .. } | HeaderV0Versioned::V2 { recovery_dtbo_addr, .. } => *recovery_dtbo_addr = offset,
    }
}

/// `len` rounded up to a whole number of `page`s.
fn align(len: u64, page: u64) -> u64 {
    if page == 0 {
        return 0;
    }
    len.div_ceil(page) * page
}

fn update_hash(header: &mut HeaderV0, kernel: &[u8], ramdisk: &[u8], extra: &[u8]) -> Result<()> {
    let page = header.page_size as usize;
    let (recovery_dtbo_size, dtb_size) = match header.versioned {
        HeaderV0Versioned::V0 => (0, 0),
        HeaderV0Versioned::V1 { recovery_dtbo_size, .. } => (recovery_dtbo_size, 0),
        HeaderV0Versioned::V2 { recovery_dtbo_size, dtb_size, .. } => (recovery_dtbo_size, dtb_size),
    };
    let mut at = 0;
    let second = part(extra, &mut at, header.second_bootloader_size as usize, page)?;
    let recovery_dtbo = part(extra, &mut at, recovery_dtbo_size as usize, page)?;
    let dtb = part(extra, &mut at, dtb_size as usize, page)?;

    let mut hash = Sha1::new();
    for (data, size) in [
        (kernel, header.kernel_size),
        (ramdisk, header.ramdisk_size),
        (second, header.second_bootloader_size),
        (recovery_dtbo, recovery_dtbo_size),
        (dtb, dtb_size),
    ] {
        hash.update(data);
        hash.update(size.to_le_bytes());
    }
    header.hash_digest = [0; 32];
    header.hash_digest[..20].copy_from_slice(&hash.finalize());
    Ok(())
}

/// The next `len` bytes of a version 2 and older image's trailing sections,
/// each padded to a page.
fn part<'a>(extra: &'a [u8], at: &mut usize, len: usize, page: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(len).context("a boot image section is too long")?;
    let data = extra.get(*at..end).context("the sections after the ramdisk are shorter than its header says")?;
    *at = end.div_ceil(page) * page;
    Ok(data)
}

fn pad(out: &mut Vec<u8>, page: usize) {
    out.resize(out.len().div_ceil(page) * page, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 4096;

    fn put(header: &mut [u8], at: usize, value: u32) {
        header[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// A version 2 image: a kernel, a ramdisk, a recovery DTBO and a DTB,
    /// laid out as the header says, with the DTBO's offset left where the
    /// stock image had it.
    fn v2_image(ramdisk: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; PAGE];
        bytes[..8].copy_from_slice(b"ANDROID!");
        put(&mut bytes, 8, 20); // kernel size
        put(&mut bytes, 12, 0x1000_0000);
        put(&mut bytes, 16, ramdisk.len() as u32);
        put(&mut bytes, 20, 0x1100_0000);
        put(&mut bytes, 32, 0x1200_0000); // kernel tags
        put(&mut bytes, 36, PAGE as u32);
        put(&mut bytes, 40, 2); // header version
        put(&mut bytes, 1632, 8); // recovery DTBO size
        put(&mut bytes, 1636, 0); // recovery DTBO offset, for packing to correct
        put(&mut bytes, 1644, 1660); // header size
        put(&mut bytes, 1648, 8); // DTB size
        bytes[1652..1660].copy_from_slice(&0x1300_0000u64.to_le_bytes());
        pad(&mut bytes, PAGE);
        bytes.extend([0x11; 20]);
        pad(&mut bytes, PAGE);
        bytes.extend(ramdisk);
        pad(&mut bytes, PAGE);
        bytes.extend(b"DTBOtest");
        pad(&mut bytes, PAGE);
        bytes.extend(b"DTBtest!");
        pad(&mut bytes, PAGE);
        bytes
    }

    #[test]
    fn a_repacked_recovery_points_at_its_recovery_dtbo() {
        let image = v2_image(&[0x22; 32]);
        let mut boot = BootImage::parse(&image).unwrap();
        boot.ramdisk = vec![0x33; 9000];
        let bytes = boot.to_bytes().unwrap();
        let offset = u64::from_le_bytes(bytes[1636..1644].try_into().unwrap()) as usize;
        assert_eq!(offset, 5 * PAGE, "the offset follows the longer ramdisk");
        assert_eq!(&bytes[offset..offset + 8], b"DTBOtest");
    }
}
