//! What arrives over sideload: an A/B update package, a zip with an
//! install script, a partition image, a system to install beside the
//! installed one, or something this recovery cannot install; and whether
//! an update package may go on this phone, checked the way Android's
//! recovery checks it.

use crate::image::{self, Image};
use crate::Refusal;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, SeekFrom};
use zip::{CompressionMethod, ZipArchive};

const PAYLOAD: &str = "payload.bin";
const PROPERTIES: &str = "payload_properties.txt";
const METADATA: &str = "META-INF/com/android/metadata";
/// The install script of a zip that installs itself.
pub const UPDATE_BINARY: &str = "META-INF/com/google/android/update-binary";

/// What a zip starts with.
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Package {
    Update(Update),
    /// A file that is no zip: an image, of a kind what it holds tells, or
    /// of none this recovery knows.
    Image(Image),
    /// A zip that installs itself by running the script inside it.
    Script,
    /// A system to install beside the installed one.
    System(skyward_systems::package::Contents),
    Unknown,
}

/// An A/B update package: the payload update_engine applies, where it
/// starts in the zip, and the package's own description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub payload_offset: u64,
    pub payload_size: u64,
    /// payload_properties.txt, handed to update_engine as its headers.
    pub properties: String,
    pub metadata: Metadata,
    /// Packages beside the main payload, for partitions it leaves out.
    pub extras: Vec<Extra>,
    /// The phone's dynamic app image package, which writes files in the
    /// data partition after the system's partitions are in.
    pub dynamic: Option<Dyn>,
}

/// A dynamic app image package: a zip with an install script of its own,
/// such as vivo's `dyn_*.zip`, which writes `/data/vivo-common/dyn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dyn {
    /// The package's name in the outer zip.
    pub name: String,
    /// The package itself, for its install script to read.
    pub zip: Vec<u8>,
}

/// A package beside the main payload in a full update package: the phone
/// maker's OEM and VGC payloads, which carry partitions of their own and
/// are installed in the same update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    /// The package's name in the outer zip, such as
    /// `oem_zip/oem_variant.zip`.
    pub name: String,
    pub payload_offset: u64,
    pub payload_size: u64,
    /// payload_properties.txt, as for the main payload.
    pub properties: String,
    /// The package itself, for update_engine to read its payload from.
    pub zip: Vec<u8>,
}

/// The packages beside the main payload to install, by name in the outer
/// zip, or the names to fall back to when the phone does not say which
/// ones it has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Select<'a> {
    pub oem: Option<&'a str>,
    pub vgc: Option<&'a str>,
    pub dynamic: Option<&'a str>,
    pub identity: Option<&'a Identity>,
}

/// What the phone says it is, for choosing the OEM and VGC packages a full
/// package carries when the outer zip names more than one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// ro.vivo.oem.name, the OEM package's own name.
    pub oem: Option<String>,
    /// ro.product.model, the model the OEM package is made for.
    pub model: Option<String>,
    /// ro.vivo.op.entry, the carrier or open-market entry.
    pub op_entry: Option<String>,
    /// ro.product.customize.comercial, the carrier customization.
    pub comercial: Option<String>,
    /// ro.product.country.region or ro.vivo.system.region.version.
    pub region: Option<String>,
    /// ro.vivo.system.region.version, which names the package set rather
    /// than the product's country.
    pub region_version: Option<String>,
    /// ro.vivo.dyn.type.
    pub dyn_type: Option<String>,
    /// ro.hardware.bbk or ro.hardware.
    pub hardware: Option<String>,
    /// ro.vgc.cust.name.
    pub vgc_cust: Option<String>,
    /// ro.vgc.device.name.
    pub vgc_device: Option<String>,
}

impl Update {
    /// The version the package installs, as its build names it.
    pub fn version(&self) -> Option<&str> {
        self.metadata.get("post-build-incremental")
    }

    /// Whether the package asks update_engine to powerwash user data.
    pub fn powerwash(&self) -> bool {
        self.properties.lines().any(|line| line.trim() == "POWERWASH=1")
    }

    /// Whether the system it installs needs all user data erased before it
    /// starts, as the package says in its description or in what it hands
    /// update_engine.
    pub fn wipes(&self) -> bool {
        self.metadata.get("ota-wipe") == Some("yes") || self.powerwash()
    }
}

/// The package's META-INF/com/android/metadata: one `key=value` a line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata(BTreeMap<String, String>);

impl Metadata {
    pub fn parse(text: &str) -> Metadata {
        Metadata(
            text.lines()
                .filter_map(|line| line.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect(),
        )
    }

    /// A value, unless it is missing or empty.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }
}

/// Tell what `file` holds from its first bytes and, for a zip, its
/// directory, taking no packages beside a full package's main payload.
pub fn identify<R: Read + Seek>(file: R) -> Result<Package> {
    identify_selected(file, Select::default())
}

/// Tell what `file` holds, taking from a full package the packages `select`
/// names beside its main payload.
pub fn identify_selected<R: Read + Seek>(mut file: R, select: Select) -> Result<Package> {
    let mut head = Vec::with_capacity(ZIP_MAGIC.len());
    (&mut file).take(ZIP_MAGIC.len() as u64).read_to_end(&mut head).context("read the package's first bytes")?;
    if head != ZIP_MAGIC {
        let len = file.seek(SeekFrom::End(0)).context("find the package's size")?;
        return image::open(&mut file, len).map(Package::Image);
    }
    file.seek(SeekFrom::Start(0)).context("rewind the package")?;
    let mut zip = ZipArchive::new(file).context("read the zip directory")?;
    if let Some(prefix) = update_prefix(&zip) {
        return update(&mut zip, &prefix, select).map(Package::Update);
    }
    if skyward_systems::package::is_system(&zip) {
        return skyward_systems::package::read(&mut zip).map(Package::System);
    }
    if zip.index_for_name(UPDATE_BINARY).is_some() {
        return Ok(Package::Script);
    }
    Ok(Package::Unknown)
}

/// The folder an update's files are in, with its trailing slash: empty
/// where they sit at the zip's top, or the folder's name where the package
/// wraps them in one, as vivo's full packages do.
fn update_prefix<R: Read + Seek>(zip: &ZipArchive<R>) -> Option<String> {
    if zip.index_for_name(PAYLOAD).is_some() {
        return Some(String::new());
    }
    zip.file_names()
        .filter_map(|name| name.strip_suffix(PAYLOAD))
        .filter(|prefix| !prefix.is_empty())
        .find(|prefix| {
            zip.index_for_name(&format!("{prefix}{PROPERTIES}")).is_some()
                && zip.index_for_name(&format!("{prefix}{METADATA}")).is_some()
        })
        .map(str::to_string)
}

fn update<R: Read + Seek>(zip: &mut ZipArchive<R>, prefix: &str, select: Select) -> Result<Update> {
    let properties = text(zip, &format!("{prefix}{PROPERTIES}"))?;
    let metadata = Metadata::parse(&text(zip, &format!("{prefix}{METADATA}"))?);
    let payload = zip.by_name(&format!("{prefix}{PAYLOAD}")).context("open payload.bin")?;
    // update_engine reads the payload straight out of the zip, so it has
    // to be stored as is.
    if payload.compression() != CompressionMethod::Stored {
        bail!("payload.bin is compressed ({:?})", payload.compression());
    }
    let payload_offset = payload.data_start().context("payload.bin has no data offset")?;
    let payload_size = payload.size();
    drop(payload);
    let identity = selected_identity(select, &metadata);
    let extras = extras(zip, prefix, select, &metadata, &identity)?;
    let dynamic = choose_dyn(zip, prefix, select, &identity)?;
    Ok(Update { payload_offset, payload_size, properties, metadata, extras, dynamic })
}

/// What the phone and the package together say it is.
fn selected_identity(select: Select, update: &Metadata) -> Identity {
    let mut identity = select.identity.cloned().unwrap_or_default();
    if identity.region_version.is_none() {
        identity.region_version = control_value(update, "ro.vivo.system.region.version");
    }
    if identity.vgc_cust.is_none() {
        identity.vgc_cust = control_value(update, "ro.vgc.cust.name");
    }
    if identity.hardware.is_none() {
        identity.hardware = update.get("hardware-device").map(str::to_string);
    }
    identity
}

/// The two kinds of package a full vivo package carries beside its main
/// payload.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Oem,
    Vgc,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Oem => "OEM",
            Kind::Vgc => "VGC",
        }
    }

    fn file_prefix(self) -> &'static str {
        match self {
            Kind::Oem => "oem_",
            Kind::Vgc => "vgc_",
        }
    }

    fn prop_file(self) -> &'static str {
        match self {
            Kind::Oem => "oem.prop",
            Kind::Vgc => "vgc.prop",
        }
    }

    fn identity_name(self, identity: &Identity) -> Option<&str> {
        match self {
            Kind::Oem => identity.oem.as_deref(),
            Kind::Vgc => identity.vgc_device.as_deref(),
        }
    }

    fn fallback<'a>(self, select: Select<'a>) -> Option<&'a str> {
        match self {
            Kind::Oem => select.oem,
            Kind::Vgc => select.vgc,
        }
    }

    /// Whether `info` describes a package the phone could be.
    fn agrees(self, info: &Metadata, identity: &Identity, update: &Metadata) -> bool {
        match self {
            Kind::Oem => {
                agrees(info, "ro.vivo.oem.name", identity.oem.as_deref())
                    && agrees(info, "ro.product.model", identity.model.as_deref())
                    && agrees(info, "ro.vivo.op.entry", identity.op_entry.as_deref())
                    && agrees(info, "ro.product.customize.comercial", identity.comercial.as_deref())
                    && agrees(info, "ro.product.country.region", identity.region.as_deref())
                    && agrees(info, "ro.vivo.system.region.version", identity.region_version.as_deref())
                    && agrees(info, "ro.vivo.dyn.type", identity.dyn_type.as_deref())
                    && agrees(info, "ro.hardware.bbk", identity.hardware.as_deref())
            }
            Kind::Vgc => {
                agrees(info, "ro.vgc.device.name", identity.vgc_device.as_deref())
                    && agrees(info, "ro.vgc.cust.name", identity.vgc_cust.as_deref())
                    && agrees(info, "hardware-device", identity.hardware.as_deref())
                    && matches_version(info, update)
            }
        }
    }
}

/// A field both sides name, and that differs, rules the package out; a
/// field either side leaves out says nothing.
fn agrees(info: &Metadata, key: &str, wanted: Option<&str>) -> bool {
    match (info.get(key), wanted) {
        (Some(found), Some(wanted)) => found == wanted,
        _ => true,
    }
}

/// A VGC package made for the build the main payload installs.
fn matches_version(info: &Metadata, update: &Metadata) -> bool {
    match (info.get("ro.vgc.system.product.version"), update.get("post-version")) {
        (Some(made_for), Some(post)) => post.starts_with(made_for),
        _ => true,
    }
}

/// Read the packages beside the main payload, choosing the one the phone
/// says it is where the outer zip carries more than one.
fn extras<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    prefix: &str,
    select: Select,
    update: &Metadata,
    identity: &Identity,
) -> Result<Vec<Extra>> {
    let mut taken = Vec::new();
    for kind in [Kind::Oem, Kind::Vgc] {
        if let Some(extra) = choose(zip, prefix, select, update, identity, kind)? {
            taken.push(extra);
        }
    }
    Ok(taken)
}

/// A value the package gives for a property in `ota-control`.
fn control_value(update: &Metadata, name: &str) -> Option<String> {
    update.get("ota-control")?.split('|').find_map(|part| {
        let fields: Vec<_> = part.split(',').map(str::trim).collect();
        if fields.first().copied() != Some(name) {
            return None;
        }
        fields.last().copied().filter(|value| !value.is_empty() && *value != "none").map(str::to_string)
    })
}

/// Choose the dynamic app image package, if the outer zip carries one, by
/// what the phone says its own is. One it cannot tell apart from another is
/// an error, not a guess.
fn choose_dyn<R: Read + Seek>(zip: &mut ZipArchive<R>, prefix: &str, select: Select, identity: &Identity) -> Result<Option<Dyn>> {
    let names = dyn_candidates(zip, prefix);
    if names.is_empty() {
        if let Some(name) = select.dynamic {
            bail!("the dynamic package {name} is not in the outer zip");
        }
        return Ok(None);
    }
    let mut matching = Vec::new();
    let mut seen = Vec::new();
    for name in names {
        let (package, info) = read_dyn(zip, &name)?;
        let base = name.rsplit('/').next().unwrap_or(&name).to_string();
        seen.push(base.clone());
        if select.dynamic.is_some_and(|wanted| wanted != name && wanted != base) {
            continue;
        }
        if let Some(wanted) = identity.dyn_type.as_deref()
            && info.get("ro.vivo.dyn.name") != Some(wanted)
        {
            continue;
        }
        matching.push(package);
    }
    match matching.len() {
        0 => bail!(
            "cannot tell which dynamic package this phone needs; the package carries {}",
            seen.join(", ")
        ),
        1 => Ok(matching.pop()),
        _ => {
            if let Some(name) = select.dynamic {
                let found = matching.iter().find(|package| package.name == name || package.name.ends_with(&format!("/{name}")));
                if let Some(package) = found {
                    return Ok(Some(package.clone()));
                }
            }
            bail!("more than one dynamic package could be this phone's: {}", matching.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))
        }
    }
}

/// The dynamic app image packages in the outer zip.
fn dyn_candidates<R: Read + Seek>(zip: &ZipArchive<R>, prefix: &str) -> Vec<String> {
    let mut dirs = vec!["oem_zip".to_string()];
    if !prefix.is_empty() {
        dirs.push(format!("{prefix}oem_zip"));
    }
    let mut names = Vec::new();
    for name in zip.file_names() {
        let Some((dir, file)) = name.rsplit_once('/') else { continue };
        if !dirs.iter().any(|wanted| wanted == dir) {
            continue;
        }
        if file.starts_with("dyn_") && file.ends_with(".zip") && !names.contains(&name.to_string()) {
            names.push(name.to_string());
        }
    }
    names
}

/// Read a dynamic app image package and the properties it describes itself
/// by.
fn read_dyn<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<(Dyn, Metadata)> {
    let mut entry = zip.by_name(name).with_context(|| format!("open the dynamic package {name}"))?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes).with_context(|| format!("read the dynamic package {name}"))?;
    drop(entry);
    let mut inner = ZipArchive::new(Cursor::new(bytes.as_slice())).with_context(|| format!("read the dynamic package {name}"))?;
    let mut described = String::new();
    for file in ["dyn.prop", METADATA] {
        if let Ok(mut entry) = inner.by_name(file) {
            let mut text = String::new();
            entry.read_to_string(&mut text).with_context(|| format!("read {file} of {name}"))?;
            described.push_str(&text);
            described.push('\n');
        }
    }
    drop(inner);
    Ok((Dyn { name: name.to_string(), zip: bytes }, Metadata::parse(&described)))
}

/// Choose one of the packages of `kind`: the named one if the zip has it,
/// else the only one the phone could be, else the one `select` falls back
/// to. More than one possibility without a fallback is an error, not a
/// guess.
fn choose<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    prefix: &str,
    select: Select,
    update: &Metadata,
    identity: &Identity,
    kind: Kind,
) -> Result<Option<Extra>> {
    let names = candidates(zip, prefix, kind);
    let fallback = fallback_name(zip, prefix, select, kind);
    if names.is_empty() {
        if let Some(name) = kind.fallback(select) {
            bail!("the {} package {name} is not in the outer zip", kind.label());
        }
        return Ok(None);
    }
    let allowed = allowed_names(update, kind);
    let mut matching = Vec::new();
    let mut fallback_extra = None;
    let mut seen = Vec::new();
    for name in names {
        let base = name.rsplit('/').next().unwrap_or(&name);
        seen.push(base.to_string());
        let (extra, info) = read_extra(zip, &name, kind)?;
        if !allowed.is_empty() {
            let Some(declared) = info.get("ro.vivo.oem.name") else { continue };
            if !allowed.contains(&declared) {
                continue;
            }
        }
        if fallback.as_deref() == Some(name.as_str()) && kind.agrees(&info, identity, update) {
            fallback_extra = Some(extra.clone());
        }
        if kind.agrees(&info, identity, update) {
            matching.push((name.clone(), extra, info));
        }
    }
    // The name the phone gives its own package is exact; anything else
    // that claims to be this phone cannot be trusted over it.
    if let Some(wanted) = kind.identity_name(identity) {
        let key = match kind {
            Kind::Oem => "ro.vivo.oem.name",
            Kind::Vgc => "ro.vgc.device.name",
        };
        matching.retain(|(_, _, info)| info.get(key) == Some(wanted));
        if matching.is_empty() {
            bail!(
                "the phone says its {} package is {wanted}, but the package carries {}",
                kind.label(),
                if seen.is_empty() { "none".to_string() } else { seen.join(", ") }
            );
        }
    }
    match matching.len() {
        1 => Ok(Some(matching.pop().expect("one match").1)),
        0 => match fallback_extra {
            Some(extra) => Ok(Some(extra)),
            None => bail!(
                "cannot tell which {} package this phone needs; the package carries {}",
                kind.label(),
                if seen.is_empty() { "none".to_string() } else { seen.join(", ") }
            ),
        },
        _ => {
            if let Some(extra) = fallback_extra {
                return Ok(Some(extra));
            }
            let names: Vec<_> = matching.into_iter().map(|(name, _, _)| name).collect();
            bail!("more than one {} package could be this phone's: {}", kind.label(), names.join(", "))
        }
    }
}

/// The inner packages of `kind` in the outer zip.
fn candidates<R: Read + Seek>(zip: &ZipArchive<R>, prefix: &str, kind: Kind) -> Vec<String> {
    let mut dirs = vec!["oem_zip".to_string()];
    if !prefix.is_empty() {
        dirs.push(format!("{prefix}oem_zip"));
    }
    let mut names = Vec::new();
    for name in zip.file_names() {
        let Some((dir, file)) = name.rsplit_once('/') else { continue };
        if !dirs.iter().any(|wanted| wanted == dir) {
            continue;
        }
        if file.starts_with(kind.file_prefix()) && file.ends_with(".zip") && !names.contains(&name.to_string()) {
            names.push(name.to_string());
        }
    }
    names
}

/// The package `select` names, as it is called in the outer zip.
fn fallback_name<R: Read + Seek>(zip: &ZipArchive<R>, prefix: &str, select: Select, kind: Kind) -> Option<String> {
    let wanted = kind.fallback(select)?;
    if zip.index_for_name(wanted).is_some() {
        return Some(wanted.to_string());
    }
    let at = format!("{prefix}{wanted}");
    zip.index_for_name(&at).is_some().then_some(at)
}

/// The OEM names the package says it is for, from `vota-oem`.
fn allowed_names(update: &Metadata, kind: Kind) -> Vec<&str> {
    if kind != Kind::Oem {
        return Vec::new();
    }
    update
        .get("vota-oem")
        .map(|names| names.split(',').map(str::trim).filter(|name| !name.is_empty()).collect())
        .unwrap_or_default()
}

/// Read one inner package, with the properties it describes itself by.
fn read_extra<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str, kind: Kind) -> Result<(Extra, Metadata)> {
    let mut entry = zip.by_name(name).with_context(|| format!("open the {} package {name}", kind.label()))?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes).with_context(|| format!("read the {} package {name}", kind.label()))?;
    drop(entry);
    let mut inner = ZipArchive::new(Cursor::new(bytes.as_slice())).with_context(|| format!("read the {} package {name}", kind.label()))?;
    let mut described = String::new();
    for file in [kind.prop_file(), METADATA] {
        if let Ok(mut entry) = inner.by_name(file) {
            let mut text = String::new();
            entry.read_to_string(&mut text).with_context(|| format!("read {file} of {name}"))?;
            described.push_str(&text);
            described.push('\n');
        }
    }
    let info = Metadata::parse(&described);
    let properties = text(&mut inner, PROPERTIES).with_context(|| format!("read the {} package's headers", kind.label()))?;
    let payload = inner.by_name(PAYLOAD).with_context(|| format!("open the {} package's payload", kind.label()))?;
    if payload.compression() != CompressionMethod::Stored {
        bail!("the {} package's payload.bin is compressed ({:?})", kind.label(), payload.compression());
    }
    let payload_offset = payload.data_start().with_context(|| format!("the {} package's payload has no data offset", kind.label()))?;
    let payload_size = payload.size();
    drop(payload);
    drop(inner);
    Ok((Extra { name: name.to_string(), payload_offset, payload_size, properties, zip: bytes }, info))
}

fn text<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<String> {
    let mut entry = zip.by_name(name).with_context(|| format!("open {name}"))?;
    let mut s = String::new();
    entry.read_to_string(&mut s).with_context(|| format!("read {name}"))?;
    Ok(s)
}

/// What the phone is, and the system installed on it now when its
/// partitions could be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Phone {
    /// ro.product.device
    pub device: String,
    /// ro.serialno
    pub serial: String,
    pub installed: Option<Build>,
}

/// The installed system, from its build.prop files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    /// ro.build.date.utc
    pub timestamp: i64,
    /// ro.build.version.security_patch, as yyyy-mm-dd
    pub security_patch: String,
}

/// Whether `update` may go on `phone`.
pub fn check(update: &Update, phone: &Phone) -> Result<(), Refusal> {
    let m = &update.metadata;
    let kind = m.get("ota-type").unwrap_or_default();
    if kind != "AB" {
        return Err(Refusal::NotAb(kind.to_string()));
    }
    let devices = m.get("pre-device").unwrap_or_default();
    if !listed(&phone.device, devices) {
        return Err(Refusal::OtherDevice(devices.to_string()));
    }
    if let Some(serials) = m.get("serialno")
        && !listed(&phone.serial, serials)
    {
        return Err(Refusal::OtherSerial);
    }
    if m.get("pre-build").is_some() || m.get("pre-build-incremental").is_some() {
        return Err(Refusal::Incremental);
    }
    let Some(build) = &phone.installed else { return Ok(()) };
    // A full package may install the version already there again. Going
    // back is allowed only when the package asks for a powerwash, so the
    // older system never reads data written by the newer one.
    let timestamp = m.get("post-timestamp").and_then(|t| t.parse().ok()).unwrap_or(0);
    if timestamp < build.timestamp && !update.powerwash() {
        return Err(Refusal::Older { package: timestamp, installed: build.timestamp });
    }
    // Patch levels are yyyy-mm-dd, so they order as text. Data that is
    // erased before the older system starts is never read under it.
    if let Some(patch) = m.get("post-security-patch-level") {
        let allowed = m.get("spl-downgrade") == Some("yes") || m.get("ota-downgrade") == Some("yes") || update.wipes();
        if patch < build.security_patch.as_str() && !allowed {
            return Err(Refusal::OlderPatch { package: patch.to_string(), installed: build.security_patch.clone() });
        }
    }
    Ok(())
}

/// Whether `name` is one of the `|`-separated names in `list`.
fn listed(name: &str, list: &str) -> bool {
    list.split('|').any(|n| n.trim() == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    const METADATA_TEXT: &str = "\
ota-property-files=payload_metadata.bin:2365:94208,payload.bin:2365:7103041280,payload_properties.txt:7103043714:154,metadata:69:637,metadata.pb:773:1529
ota-required-cache=0
ota-type=AB
post-build=Xiaomi/ishtar/ishtar:16/BP2A.250605.031.A3/OS3.0.306.0.WMACNXM:user/release-keys
post-build-incremental=OS3.0.306.0.WMACNXM
post-sdk-level=36
post-security-patch-level=2026-06-01
post-timestamp=1781000000
pre-device=ishtar
";

    const PROPERTIES_TEXT: &str = "\
FILE_HASH=7yP3vJ8M1qfAeMIuSv8Bq6P8xHkZ8eCwI0G2vC9q8Y4=
FILE_SIZE=4096
METADATA_HASH=sV9V3cOa1f8T5Nn+Yt0e8b1wqM3k9yvX2hR6J4iQ7nI=
METADATA_SIZE=1024
";

    fn phone() -> Phone {
        Phone {
            device: "ishtar".into(),
            serial: "2674dc3e".into(),
            installed: Some(Build { timestamp: 1_778_243_655, security_patch: "2026-04-01".into() }),
        }
    }

    fn update_with(metadata: &str, properties: &str) -> Update {
        Update {
            payload_offset: 0,
            payload_size: 0,
            properties: properties.into(),
            metadata: Metadata::parse(metadata),
            extras: Vec::new(),
            dynamic: None,
        }
    }

    fn zip_with(entries: &[(&str, &[u8], CompressionMethod)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data, method) in entries {
            w.start_file(*name, SimpleFileOptions::default().compression_method(*method)).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn a_full_package_for_this_phone_is_accepted() {
        assert_eq!(check(&update_with(METADATA_TEXT, PROPERTIES_TEXT), &phone()), Ok(()));
    }

    #[test]
    fn the_same_build_may_be_installed_again() {
        let same = METADATA_TEXT.replace("1781000000", "1778243655");
        assert_eq!(check(&update_with(&same, PROPERTIES_TEXT), &phone()), Ok(()));
    }

    #[test]
    fn packages_for_other_phones_are_refused() {
        let other = METADATA_TEXT.replace("pre-device=ishtar", "pre-device=nezha");
        assert_eq!(check(&update_with(&other, PROPERTIES_TEXT), &phone()), Err(Refusal::OtherDevice("nezha".into())));
        let listed = METADATA_TEXT.replace("pre-device=ishtar", "pre-device=nezha|ishtar");
        assert_eq!(check(&update_with(&listed, PROPERTIES_TEXT), &phone()), Ok(()));
        let serial = format!("{METADATA_TEXT}serialno=abc|def\n");
        assert_eq!(check(&update_with(&serial, PROPERTIES_TEXT), &phone()), Err(Refusal::OtherSerial));
        let ours = format!("{METADATA_TEXT}serialno=abc|2674dc3e\n");
        assert_eq!(check(&update_with(&ours, PROPERTIES_TEXT), &phone()), Ok(()));
        let block = METADATA_TEXT.replace("ota-type=AB", "ota-type=BLOCK");
        assert_eq!(check(&update_with(&block, PROPERTIES_TEXT), &phone()), Err(Refusal::NotAb("BLOCK".into())));
    }

    #[test]
    fn incremental_and_older_packages_are_refused() {
        let incremental = format!(
            "{METADATA_TEXT}pre-build=Xiaomi/ishtar/ishtar:16/X/OS3.0.305.0.WMACNXM:user/release-keys\npre-build-incremental=OS3.0.305.0.WMACNXM\n"
        );
        assert_eq!(check(&update_with(&incremental, PROPERTIES_TEXT), &phone()), Err(Refusal::Incremental));
        let older = METADATA_TEXT.replace("1781000000", "1770000000");
        assert_eq!(
            check(&update_with(&older, PROPERTIES_TEXT), &phone()),
            Err(Refusal::Older { package: 1_770_000_000, installed: 1_778_243_655 })
        );
        let patch = METADATA_TEXT.replace("2026-06-01", "2026-03-01");
        assert_eq!(
            check(&update_with(&patch, PROPERTIES_TEXT), &phone()),
            Err(Refusal::OlderPatch { package: "2026-03-01".into(), installed: "2026-04-01".into() })
        );
        let allowed = format!("{}spl-downgrade=yes\n", METADATA_TEXT.replace("2026-06-01", "2026-03-01"));
        assert_eq!(check(&update_with(&allowed, PROPERTIES_TEXT), &phone()), Ok(()));
    }

    #[test]
    fn an_older_package_may_powerwash_but_nothing_else_lets_it_go_back() {
        let older = METADATA_TEXT.replace("1781000000", "1770000000");
        let expected = Err(Refusal::Older { package: 1_770_000_000, installed: 1_778_243_655 });
        assert_eq!(check(&update_with(&older, PROPERTIES_TEXT), &phone()), expected);
        let wipe_only = format!("{older}ota-wipe=yes\n");
        assert_eq!(
            check(&update_with(&wipe_only, PROPERTIES_TEXT), &phone()),
            expected,
            "a wipe from metadata alone does not authorize an older build"
        );
        let powerwash = format!("{PROPERTIES_TEXT}POWERWASH=1\n");
        let update = update_with(&older, &powerwash);
        assert!(update.powerwash());
        assert!(update.wipes());
        assert_eq!(check(&update, &phone()), Ok(()));
        let no_powerwash = format!("{PROPERTIES_TEXT}POWERWASH=0\n");
        assert_eq!(check(&update_with(&older, &no_powerwash), &phone()), expected);
    }

    #[test]
    fn a_package_may_ask_for_all_user_data_to_be_erased_either_way() {
        let plain = update_with(METADATA_TEXT, PROPERTIES_TEXT);
        assert!(!plain.wipes());
        let powerwash = update_with(METADATA_TEXT, &format!("{PROPERTIES_TEXT}POWERWASH=1\n"));
        assert!(powerwash.wipes());
        assert_eq!(check(&powerwash, &phone()), Ok(()), "erasing is asked of the user, not refused");
        let wipe = update_with(&format!("{METADATA_TEXT}ota-wipe=yes\n"), PROPERTIES_TEXT);
        assert!(wipe.wipes());
        assert!(!update_with(METADATA_TEXT, &format!("{PROPERTIES_TEXT}POWERWASH=0\n")).wipes());
        let older_patch = format!("{}ota-wipe=yes\n", METADATA_TEXT.replace("2026-06-01", "2026-03-01"));
        assert_eq!(check(&update_with(&older_patch, PROPERTIES_TEXT), &phone()), Ok(()), "the data it could not read is erased");
    }

    #[test]
    fn without_an_installed_system_only_the_package_itself_is_checked() {
        let older = METADATA_TEXT.replace("1781000000", "1770000000");
        let bare = Phone { installed: None, ..phone() };
        assert_eq!(check(&update_with(&older, PROPERTIES_TEXT), &bare), Ok(()));
    }

    #[test]
    fn an_update_zip_gives_where_its_stored_payload_starts() {
        let payload = b"CrAU\0\0\0\0\0\0\0\x02rest of the payload";
        let bytes = zip_with(&[
            ("payload.bin", payload, CompressionMethod::Stored),
            ("payload_properties.txt", PROPERTIES_TEXT.as_bytes(), CompressionMethod::Stored),
            ("META-INF/com/android/metadata", METADATA_TEXT.as_bytes(), CompressionMethod::Deflated),
        ]);
        let Package::Update(u) = identify(Cursor::new(&bytes)).unwrap() else { panic!("an update package") };
        let at = u.payload_offset as usize;
        assert_eq!(&bytes[at..at + payload.len()], payload);
        assert_eq!(u.payload_size, payload.len() as u64);
        assert_eq!(u.properties, PROPERTIES_TEXT);
        assert_eq!(u.version(), Some("OS3.0.306.0.WMACNXM"));
    }

    fn inner_package(prop_file: &str, prop: &str) -> Vec<u8> {
        zip_with(&[
            (PAYLOAD, b"CrAU\0\0\0\0\0\0\0\x02extra", CompressionMethod::Stored),
            (PROPERTIES, b"FILE_HASH=extra\nFILE_SIZE=18\n", CompressionMethod::Stored),
            (prop_file, prop.as_bytes(), CompressionMethod::Stored),
            ("META-INF/com/android/metadata", b"hardware-device=H\n", CompressionMethod::Stored),
        ])
    }

    fn full_package(extras: &[(&str, &str, &str)], metadata: &str) -> Vec<u8> {
        let mut entries: Vec<(&str, &[u8], CompressionMethod)> = vec![
            (PAYLOAD, b"CrAU\0\0\0\0\0\0\0\x02main", CompressionMethod::Stored),
            (PROPERTIES, PROPERTIES_TEXT.as_bytes(), CompressionMethod::Stored),
            (METADATA, metadata.as_bytes(), CompressionMethod::Stored),
        ];
        let inner: Vec<(&str, Vec<u8>)> = extras.iter().map(|(name, prop, value)| (*name, inner_package(prop, value))).collect();
        for (name, bytes) in &inner {
            entries.push((*name, bytes.as_slice(), CompressionMethod::Deflated));
        }
        zip_with(&entries)
    }

    #[test]
    fn the_package_the_phone_names_itself_is_chosen() {
        let oem_a = "PD_A_CN-ZH_FULL_SC_NULL";
        let oem_b = "PD_B_CN-ZH_CMCC_RWA_NULL";
        let metadata = format!("{METADATA_TEXT}vota-oem={oem_a},{oem_b}\n");
        let bytes = full_package(
            &[
                ("oem_zip/oem_a.zip", "oem.prop", &format!("ro.vivo.oem.name={oem_a}\nro.vivo.op.entry=FULL_SC\n")),
                ("oem_zip/oem_b.zip", "oem.prop", &format!("ro.vivo.oem.name={oem_b}\nro.vivo.op.entry=CMCC_RWA\n")),
            ],
            &metadata,
        );
        let identity = Identity { oem: Some(oem_b.into()), ..Identity::default() };
        let select = Select { oem: None, vgc: None, dynamic: None, identity: Some(&identity) };
        let Package::Update(update) = identify_selected(Cursor::new(&bytes), select).unwrap() else { panic!("an update package") };
        assert_eq!(update.extras.len(), 1);
        assert_eq!(update.extras[0].name, "oem_zip/oem_b.zip");
    }

    #[test]
    fn without_its_own_name_the_phone_is_matched_by_its_carrier_entry() {
        let oem_a = "PD_A_CN-ZH_FULL_SC_NULL";
        let oem_b = "PD_B_CN-ZH_CMCC_RWA_NULL";
        let bytes = full_package(
            &[
                ("oem_zip/oem_a.zip", "oem.prop", &format!("ro.vivo.oem.name={oem_a}\nro.vivo.op.entry=FULL_SC\n")),
                ("oem_zip/oem_b.zip", "oem.prop", &format!("ro.vivo.oem.name={oem_b}\nro.vivo.op.entry=CMCC_RWA\n")),
            ],
            METADATA_TEXT,
        );
        let identity = Identity { op_entry: Some("CMCC_RWA".into()), ..Identity::default() };
        let select = Select { oem: None, vgc: None, dynamic: None, identity: Some(&identity) };
        let Package::Update(update) = identify_selected(Cursor::new(&bytes), select).unwrap() else { panic!("an update package") };
        assert_eq!(update.extras[0].name, "oem_zip/oem_b.zip");
    }

    #[test]
    fn a_fallback_name_settles_what_the_phone_cannot() {
        let bytes = full_package(
            &[
                ("oem_zip/oem_a.zip", "oem.prop", "ro.vivo.oem.name=PD_A\n"),
                ("oem_zip/oem_b.zip", "oem.prop", "ro.vivo.oem.name=PD_B\n"),
            ],
            METADATA_TEXT,
        );
        let select = Select { oem: Some("oem_zip/oem_b.zip"), vgc: None, dynamic: None, identity: None };
        let Package::Update(update) = identify_selected(Cursor::new(&bytes), select).unwrap() else { panic!("an update package") };
        assert_eq!(update.extras[0].name, "oem_zip/oem_b.zip");
    }

    #[test]
    fn an_ambiguous_package_is_refused_rather_than_guessed() {
        let bytes = full_package(
            &[
                ("oem_zip/oem_a.zip", "oem.prop", "ro.vivo.oem.name=PD_A\n"),
                ("oem_zip/oem_b.zip", "oem.prop", "ro.vivo.oem.name=PD_B\n"),
            ],
            METADATA_TEXT,
        );
        let select = Select { oem: None, vgc: None, dynamic: None, identity: None };
        assert!(identify_selected(Cursor::new(&bytes), select).is_err());
    }

    #[test]
    fn the_package_says_which_vgc_customization_it_needs() {
        let bytes = full_package(
            &[
                ("oem_zip/vgc_v000.zip", "vgc.prop", "ro.vgc.cust.name=V000\n"),
                ("oem_zip/vgc_v001.zip", "vgc.prop", "ro.vgc.cust.name=V001\n"),
            ],
            &format!("{METADATA_TEXT}ota-control=ro.vgc.cust.name,none,NULL,V000|\n"),
        );
        let select = Select { oem: None, vgc: None, dynamic: None, identity: None };
        let Package::Update(update) = identify_selected(Cursor::new(&bytes), select).unwrap() else { panic!("an update package") };
        assert_eq!(update.extras.len(), 1);
        assert_eq!(update.extras[0].name, "oem_zip/vgc_v000.zip");
    }

    #[test]
    fn the_dynamic_package_the_phone_has_is_chosen() {
        let dyn_null = zip_with(&[
            ("dyn.prop", b"ro.vivo.dyn.name=NULL\n".as_slice(), CompressionMethod::Stored),
            ("META-INF/com/android/metadata", b"package-type=full,dyn\n".as_slice(), CompressionMethod::Stored),
        ]);
        let dyn_n = zip_with(&[
            ("dyn.prop", b"ro.vivo.dyn.name=DYNN\n".as_slice(), CompressionMethod::Stored),
            ("META-INF/com/android/metadata", b"package-type=full,dyn\n".as_slice(), CompressionMethod::Stored),
        ]);
        let bytes = zip_with(&[
            (PAYLOAD, b"CrAU\0\0\0\0\0\0\0\x02main".as_slice(), CompressionMethod::Stored),
            (PROPERTIES, PROPERTIES_TEXT.as_bytes(), CompressionMethod::Stored),
            (METADATA, METADATA_TEXT.as_bytes(), CompressionMethod::Stored),
            ("oem_zip/dyn_PD_DEVICE_NULL.zip", dyn_null.as_slice(), CompressionMethod::Deflated),
            ("oem_zip/dyn_PD_DEVICE_DYNN.zip", dyn_n.as_slice(), CompressionMethod::Deflated),
        ]);
        let identity = Identity { dyn_type: Some("NULL".into()), ..Identity::default() };
        let select = Select { oem: None, vgc: None, dynamic: None, identity: Some(&identity) };
        let Package::Update(update) = identify_selected(Cursor::new(&bytes), select).unwrap() else { panic!("an update package") };
        assert_eq!(update.dynamic.as_ref().map(|package| package.name.as_str()), Some("oem_zip/dyn_PD_DEVICE_NULL.zip"));
    }

    #[test]
    fn a_configured_dynamic_package_settles_a_package_without_a_dyn_type() {
        let dyn_null = zip_with(&[
            ("dyn.prop", b"ro.vivo.dyn.name=NULL\n".as_slice(), CompressionMethod::Stored),
            ("META-INF/com/android/metadata", b"package-type=full,dyn\n".as_slice(), CompressionMethod::Stored),
        ]);
        let dyn_n = zip_with(&[
            ("dyn.prop", b"ro.vivo.dyn.name=DYNN\n".as_slice(), CompressionMethod::Stored),
            ("META-INF/com/android/metadata", b"package-type=full,dyn\n".as_slice(), CompressionMethod::Stored),
        ]);
        let bytes = zip_with(&[
            (PAYLOAD, b"CrAU\0\0\0\0\0\0\0\x02main".as_slice(), CompressionMethod::Stored),
            (PROPERTIES, PROPERTIES_TEXT.as_bytes(), CompressionMethod::Stored),
            (METADATA, METADATA_TEXT.as_bytes(), CompressionMethod::Stored),
            ("oem_zip/dyn_PD_DEVICE_NULL.zip", dyn_null.as_slice(), CompressionMethod::Deflated),
            ("oem_zip/dyn_PD_DEVICE_DYNN.zip", dyn_n.as_slice(), CompressionMethod::Deflated),
        ]);
        let select = Select { oem: None, vgc: None, dynamic: Some("dyn_PD_DEVICE_DYNN.zip"), identity: None };
        let Package::Update(update) = identify_selected(Cursor::new(&bytes), select).unwrap() else { panic!("an update package") };
        assert_eq!(update.dynamic.as_ref().map(|package| package.name.as_str()), Some("oem_zip/dyn_PD_DEVICE_DYNN.zip"));
    }

    #[test]
    fn a_full_package_carries_the_named_packages_beside_its_payload() {
        let oem_payload = b"CrAU\0\0\0\0\0\0\0\x02oem";
        let oem = zip_with(&[
            (PAYLOAD, oem_payload, CompressionMethod::Stored),
            (PROPERTIES, b"FILE_HASH=oem\nFILE_SIZE=15\n", CompressionMethod::Stored),
        ]);
        let main = b"CrAU\0\0\0\0\0\0\0\x02main";
        let bytes = zip_with(&[
            (PAYLOAD, main, CompressionMethod::Stored),
            (PROPERTIES, PROPERTIES_TEXT.as_bytes(), CompressionMethod::Stored),
            (METADATA, METADATA_TEXT.as_bytes(), CompressionMethod::Deflated),
            ("oem_zip/oem_variant.zip", &oem, CompressionMethod::Deflated),
        ]);
        let select = Select { oem: Some("oem_zip/oem_variant.zip"), vgc: None, dynamic: None, identity: None };
        let Package::Update(update) = identify_selected(Cursor::new(&bytes), select).unwrap() else {
            panic!("an update package");
        };
        assert_eq!(update.extras.len(), 1);
        let extra = &update.extras[0];
        assert_eq!(extra.name, "oem_zip/oem_variant.zip");
        assert_eq!(extra.properties, "FILE_HASH=oem\nFILE_SIZE=15\n");
        assert_eq!(&extra.zip[extra.payload_offset as usize..][..oem_payload.len()], oem_payload);
        assert_eq!(extra.payload_size, oem_payload.len() as u64);
    }

    #[test]
    fn a_named_package_the_outer_zip_lacks_is_an_error() {
        let bytes = zip_with(&[
            (PAYLOAD, b"CrAU\0\0\0\0\0\0\0\x02main", CompressionMethod::Stored),
            (PROPERTIES, PROPERTIES_TEXT.as_bytes(), CompressionMethod::Stored),
            (METADATA, METADATA_TEXT.as_bytes(), CompressionMethod::Stored),
        ]);
        let select = Select { oem: Some("oem_zip/missing.zip"), vgc: None, dynamic: None, identity: None };
        assert!(identify_selected(Cursor::new(&bytes), select).is_err());
    }

    #[test]
    fn script_zips_compressed_payloads_and_other_files_are_told_apart() {
        let script = zip_with(&[("META-INF/com/google/android/update-binary", b"#!/sbin/sh\n", CompressionMethod::Deflated)]);
        assert_eq!(identify(Cursor::new(script)).unwrap(), Package::Script);
        let squeezed = zip_with(&[
            ("payload.bin", &[7u8; 4096], CompressionMethod::Deflated),
            ("payload_properties.txt", PROPERTIES_TEXT.as_bytes(), CompressionMethod::Stored),
            ("META-INF/com/android/metadata", METADATA_TEXT.as_bytes(), CompressionMethod::Stored),
        ]);
        assert!(identify(Cursor::new(squeezed)).is_err());
        let other = zip_with(&[("readme.txt", b"hello", CompressionMethod::Stored)]);
        assert_eq!(identify(Cursor::new(other)).unwrap(), Package::Unknown);
        let text = Image { kind: image::ImageKind::Other, layout: image::Layout::whole(14) };
        assert_eq!(identify(Cursor::new(b"just some text".to_vec())).unwrap(), Package::Image(text), "no zip, and nothing an image is known by");
        assert_eq!(identify(Cursor::new(Vec::new())).unwrap(), Package::Image(Image { kind: image::ImageKind::Other, layout: image::Layout::whole(0) }));
    }
}
