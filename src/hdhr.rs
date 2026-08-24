// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! SiliconDust HDHomeRun: UDP broadcast discovery on :65001 (libhdhomerun
//! wire format — type 0x0002 request / 0x0003 reply, TLV payload, trailing
//! CRC32-LE) and the HTTP JSON API (`/discover.json`, `/lineup.json`).
//! Lineup entries carry ready-to-GET MPEG-TS URLs; the tuner allocates its
//! own tuners, so recording is just an HTTP GET copied to disk.

use crate::{model::Channel, state::SettingsCache};
use anyhow::{Context, anyhow};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::{Duration, Instant},
};

pub const DISCOVER_PORT: u16 = 65001;
pub const TYPE_DISCOVER_REQ: u16 = 0x0002;
pub const TYPE_DISCOVER_RPY: u16 = 0x0003;
pub const TAG_DEVICE_TYPE: u8 = 0x01;
pub const TAG_DEVICE_ID: u8 = 0x02;
pub const TAG_BASE_URL: u8 = 0x2A;

/// Wildcard value for device type / device id in a discover request.
pub const WILDCARD: u32 = 0xFFFF_FFFF;

/// Largest packet libhdhomerun will ever send (hdhomerun_pkt.h: 3074).
const MAX_PACKET: usize = 3074;
/// Response size caps for the JSON endpoints.
const DISCOVER_JSON_LIMIT: usize = 1024 * 1024;
const LINEUP_JSON_LIMIT: usize = 8 * 1024 * 1024;
/// Per-request HTTP timeout for the tuner (it is on the LAN).
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Device {
    #[serde(rename = "DeviceID", default)]
    pub device_id: String,
    #[serde(rename = "LocalIP", default)]
    pub local_ip: String,
    #[serde(rename = "BaseURL", default)]
    pub base_url: String,
    #[serde(rename = "LineupURL", default)]
    pub lineup_url: String,
    #[serde(rename = "DeviceAuth", default)]
    pub device_auth: String,
    #[serde(rename = "TunerCount", default)]
    pub tuner_count: u32,
    #[serde(rename = "ModelNumber", default)]
    pub model_number: String,
    #[serde(rename = "FirmwareVersion", default)]
    pub firmware_version: String,
    #[serde(rename = "FriendlyName", default)]
    pub friendly_name: String,
}

#[derive(Default)]
struct Inner {
    device: Option<Device>,
    channels: Vec<Channel>,
    icons: HashMap<String, String>,
}

pub struct Client {
    pub settings: Arc<SettingsCache>,
    pub pool: PgPool,
    pub http: reqwest::Client,
    inner: RwLock<Inner>,
}

impl Client {
    pub fn new(settings: Arc<SettingsCache>, pool: PgPool, http: reqwest::Client) -> Self {
        Self { settings, pool, http, inner: RwLock::new(Inner::default()) }
    }

    /// Populate channels from the `channels` table (instant Live TV after boot).
    pub async fn load_cached(&self) -> anyhow::Result<()> {
        let chans = crate::db::channels::list(&self.pool).await?;
        let mut g = self.inner.write();
        g.icons = chans.iter().filter_map(|c| c.icon.clone().map(|i| (c.guide_number.clone(), i))).collect();
        g.channels = chans;
        Ok(())
    }

    /// Resolve the tuner (Settings.hdhr_ip wins, else UDP discovery), pull
    /// the lineup, persist it.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let started = Instant::now();
        let hdhr_ip = self.settings.get().hdhr_ip.trim().to_string();
        let device = if !hdhr_ip.is_empty() {
            fetch_discover(&self.http, &hdhr_ip)
                .await
                .with_context(|| format!("discover.json from configured tuner {hdhr_ip}"))?
        } else {
            let found = discover_udp(&self.http, Duration::from_millis(2500))
                .await
                .context("HDHomeRun broadcast discovery failed; set hdhrIp in settings")?;
            if found.len() > 1 {
                tracing::info!(
                    count = found.len(),
                    chosen = %found[0].base_url,
                    "multiple HDHomeRun devices found; using the first (set hdhrIp to pin one)"
                );
            }
            found
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no HDHomeRun found via broadcast; set hdhrIp in settings"))?
        };

        let mut channels = fetch_lineup(&self.http, &device).await?;
        {
            let g = self.inner.read();
            for c in &mut channels {
                if c.icon.is_none()
                    && let Some(icon) = g.icons.get(&c.guide_number)
                {
                    c.icon = Some(icon.clone());
                }
            }
        }
        let count = channels.len();
        {
            let mut g = self.inner.write();
            g.device = Some(device.clone());
            g.channels = channels.clone();
        }
        metrics::gauge!("ontele_livetv_channels").set(count as f64);
        tracing::info!(
            device = %device.device_id,
            base_url = %device.base_url,
            model = %device.model_number,
            tuners = device.tuner_count,
            channels = count,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "HDHomeRun lineup refreshed"
        );
        crate::db::channels::replace(&self.pool, &channels).await.context("persist channel lineup")?;
        Ok(())
    }

    pub fn device(&self) -> Option<Device> {
        self.inner.read().device.clone()
    }

    pub fn channels(&self) -> Vec<Channel> {
        self.inner.read().channels.clone()
    }

    pub fn channel(&self, guide_number: &str) -> Option<Channel> {
        self.inner.read().channels.iter().find(|c| c.guide_number == guide_number).cloned()
    }

    pub fn channel_name(&self, guide_number: &str) -> Option<String> {
        self.channel(guide_number).map(|c| c.guide_name)
    }

    /// MPEG-TS URL for a GuideNumber; falls back to `<base>/auto/v<num>`.
    pub fn stream_url(&self, guide_number: &str) -> Option<String> {
        let guide_number = guide_number.trim();
        if !is_valid_guide_number(guide_number) {
            return None;
        }
        let g = self.inner.read();
        if let Some(c) = g.channels.iter().find(|c| c.guide_number == guide_number)
            && !c.url.trim().is_empty()
        {
            return Some(c.url.trim().to_string());
        }
        let dev = g.device.as_ref()?;
        let base = normalize_base_url(&dev.base_url);
        if base.is_empty() {
            return None;
        }
        Some(format!("{base}/auto/v{guide_number}"))
    }

    /// Attach channel icons (from the XMLTV guide) and persist them.
    pub async fn set_icons(&self, icons: HashMap<String, String>) {
        if icons.is_empty() {
            return;
        }
        let persist: Vec<(String, String)> = {
            let mut g = self.inner.write();
            let mut persist = Vec::new();
            for (num, icon) in icons {
                let icon = icon.trim().to_string();
                if icon.is_empty() {
                    continue;
                }
                g.icons.insert(num.clone(), icon.clone());
                if let Some(c) = g.channels.iter_mut().find(|c| c.guide_number == num)
                    && c.icon.as_deref() != Some(icon.as_str())
                {
                    c.icon = Some(icon.clone());
                    persist.push((num, icon));
                }
            }
            persist
        };
        if persist.is_empty() {
            return;
        }
        match crate::db::channels::set_icons(&self.pool, &persist).await {
            Ok(()) => tracing::debug!(count = persist.len(), "channel icons persisted"),
            Err(e) => {
                tracing::warn!(error = %e, count = persist.len(), "persist channel icons failed")
            }
        }
    }
}

/// A GuideNumber is something like `7.1`, `1001` or (rarely) `A12`; it is
/// interpolated into a URL path, so keep it to a safe alphabet.
fn is_valid_guide_number(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// `192.168.1.20` → `http://192.168.1.20`; strips whitespace and trailing `/`.
pub fn normalize_base_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.is_empty() {
        return String::new();
    }
    if b.starts_with("http://") || b.starts_with("https://") { b.to_string() } else { format!("http://{b}") }
}

// ---------------------------------------------------------------------------
// Wire format (hdhomerun_pkt.h)
//
//   u16 BE type | u16 BE payload length | payload (TLVs) | u32 LE CRC32
//
// TLV: u8 tag, length (1 byte when < 128, else 2 bytes: 0x80|lo7, hi),
// value bytes. The CRC covers header + payload.
// ---------------------------------------------------------------------------

fn push_tlv(out: &mut Vec<u8>, tag: u8, value: &[u8]) {
    out.push(tag);
    let len = value.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        out.push(0x80 | (len & 0x7F) as u8);
        out.push((len >> 7) as u8);
    }
    out.extend_from_slice(value);
}

/// Frame a packet: header + payload + CRC32-LE.
pub fn frame_packet(ptype: u16, payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4 + payload.len() + 4);
    pkt.extend_from_slice(&ptype.to_be_bytes());
    pkt.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    pkt.extend_from_slice(payload);
    let crc = crc32fast::hash(&pkt);
    pkt.extend_from_slice(&crc.to_le_bytes());
    pkt
}

/// Wildcard discover request per hdhomerun_pkt.h.
pub fn discover_packet() -> Vec<u8> {
    let mut payload = Vec::with_capacity(12);
    push_tlv(&mut payload, TAG_DEVICE_TYPE, &WILDCARD.to_be_bytes());
    push_tlv(&mut payload, TAG_DEVICE_ID, &WILDCARD.to_be_bytes());
    frame_packet(TYPE_DISCOVER_REQ, &payload)
}

/// Validate header + CRC and return (type, payload).
fn unframe_packet(b: &[u8]) -> Option<(u16, &[u8])> {
    if b.len() < 8 {
        return None;
    }
    let ptype = u16::from_be_bytes([b[0], b[1]]);
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    let end = 4 + len;
    if b.len() < end + 4 {
        return None;
    }
    let crc_expected = u32::from_le_bytes([b[end], b[end + 1], b[end + 2], b[end + 3]]);
    if crc32fast::hash(&b[..end]) != crc_expected {
        return None;
    }
    Some((ptype, &b[4..end]))
}

/// Iterate TLVs; stops at the first malformed entry.
fn tlvs(mut payload: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        if payload.len() < 2 {
            return None;
        }
        let tag = payload[0];
        let (len, hdr) = if payload[1] & 0x80 == 0 {
            (payload[1] as usize, 2usize)
        } else {
            if payload.len() < 3 {
                return None;
            }
            (((payload[1] & 0x7F) as usize) | ((payload[2] as usize) << 7), 3usize)
        };
        if payload.len() < hdr + len {
            return None;
        }
        let value = &payload[hdr..hdr + len];
        payload = &payload[hdr + len..];
        Some((tag, value))
    })
}

/// Parse a discover reply → (device id hex, base url). Validates CRC.
pub fn parse_reply(b: &[u8]) -> Option<(String, String)> {
    let (ptype, payload) = unframe_packet(b)?;
    if ptype != TYPE_DISCOVER_RPY {
        return None;
    }
    let mut device_id = String::new();
    let mut base_url = String::new();
    for (tag, value) in tlvs(payload) {
        match tag {
            TAG_DEVICE_ID if value.len() == 4 => {
                let id = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
                device_id = format!("{id:08X}");
            }
            TAG_BASE_URL => {
                // Some firmwares NUL-terminate the string.
                let s = String::from_utf8_lossy(value);
                base_url = s.trim_end_matches('\0').trim().to_string();
            }
            _ => {}
        }
    }
    if base_url.is_empty() {
        return None;
    }
    Some((device_id, base_url))
}

/// Broadcast and collect replies for `timeout` (≈2 s).
pub async fn discover_udp(http: &reqwest::Client, timeout: Duration) -> anyhow::Result<Vec<Device>> {
    let sock = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.context("bind discovery socket")?;
    sock.set_broadcast(true).context("enable broadcast")?;
    let target = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVER_PORT));
    let pkt = discover_packet();
    sock.send_to(&pkt, target).await.context("send discover broadcast")?;

    let start = Instant::now();
    let deadline = start + timeout;
    let half = start + timeout / 2;
    let mut buf = vec![0u8; MAX_PACKET];
    // (base url, device id, reply address); Vec keeps reply order stable.
    let mut seen: Vec<(String, String, SocketAddr)> = Vec::new();
    let mut resent = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // libhdhomerun retries once; a single dropped broadcast otherwise
        // costs a full refresh cycle.
        if !resent && seen.is_empty() && now >= half {
            resent = true;
            if let Err(e) = sock.send_to(&pkt, target).await {
                tracing::debug!(error = %e, "discover re-broadcast failed");
            }
        }
        let wait = if !resent && seen.is_empty() { half - now } else { deadline - now };
        match tokio::time::timeout(wait, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Some((id, base)) = parse_reply(&buf[..n]) {
                    if !seen.iter().any(|(b, _, _)| *b == base) {
                        tracing::debug!(device = %id, base_url = %base, from = %from, "HDHomeRun reply");
                        seen.push((base, id, from));
                    }
                } else {
                    tracing::trace!(from = %from, bytes = n, "ignoring non-discover datagram");
                }
            }
            Ok(Err(e)) => {
                // Transient errors (e.g. ICMP unreachable surfaced on the
                // socket) should not abort the collection window.
                tracing::debug!(error = %e, "discover recv error");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => {
                if resent || !seen.is_empty() {
                    break;
                }
            }
        }
    }

    let mut devices = Vec::with_capacity(seen.len());
    for (base, id, from) in seen {
        let local_ip = from.ip().to_string();
        match tokio::time::timeout(HTTP_TIMEOUT, fetch_discover(http, &base)).await {
            Ok(Ok(mut d)) => {
                if d.local_ip.is_empty() {
                    d.local_ip = local_ip;
                }
                if d.device_id.is_empty() {
                    d.device_id = id;
                }
                devices.push(d);
            }
            Ok(Err(e)) => {
                tracing::warn!(device = %id, base_url = %base, error = %e, "discover.json failed; using UDP reply only");
                devices.push(Device {
                    device_id: id,
                    local_ip,
                    base_url: normalize_base_url(&base),
                    ..Device::default()
                });
            }
            Err(_) => {
                tracing::warn!(device = %id, base_url = %base, "discover.json timed out; using UDP reply only");
                devices.push(Device {
                    device_id: id,
                    local_ip,
                    base_url: normalize_base_url(&base),
                    ..Device::default()
                });
            }
        }
    }
    Ok(devices)
}

/// GET a small JSON document with a hard size cap.
async fn get_limited(http: &reqwest::Client, url: &str, limit: usize) -> anyhow::Result<bytes::Bytes> {
    let mut resp = http
        .get(url)
        .timeout(HTTP_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("GET {url}: HTTP {status}"));
    }
    if let Some(len) = resp.content_length()
        && len as usize > limit
    {
        return Err(anyhow!("GET {url}: response too large ({len} bytes > {limit})"));
    }
    let mut body = Vec::with_capacity(resp.content_length().unwrap_or(4096).min(limit as u64) as usize);
    while let Some(chunk) = resp.chunk().await.with_context(|| format!("GET {url}: read body"))? {
        if body.len() + chunk.len() > limit {
            return Err(anyhow!("GET {url}: response too large (> {limit} bytes)"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(bytes::Bytes::from(body))
}

pub async fn fetch_discover(http: &reqwest::Client, base_url: &str) -> anyhow::Result<Device> {
    let base = normalize_base_url(base_url);
    if base.is_empty() {
        return Err(anyhow!("empty tuner address"));
    }
    let url = format!("{base}/discover.json");
    let body = get_limited(http, &url, DISCOVER_JSON_LIMIT).await?;
    let mut dev: Device = serde_json::from_slice(&body).with_context(|| format!("parse {url}"))?;
    if dev.base_url.trim().is_empty() {
        dev.base_url = base;
    } else {
        dev.base_url = normalize_base_url(&dev.base_url);
    }
    if dev.local_ip.is_empty()
        && let Ok(u) = reqwest::Url::parse(&dev.base_url)
        && let Some(host) = u.host_str()
    {
        dev.local_ip = host.to_string();
    }
    Ok(dev)
}

#[derive(Debug, Deserialize)]
struct LineupEntry {
    #[serde(rename = "GuideNumber", default)]
    guide_number: String,
    #[serde(rename = "GuideName", default)]
    guide_name: String,
    #[serde(rename = "URL", default)]
    url: String,
    #[serde(rename = "HD", default, deserialize_with = "de_flag")]
    hd: i64,
}

/// The tuner emits `"HD": 1`; be lenient about booleans/strings too.
fn de_flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Flag {
        Int(i64),
        Bool(bool),
        Str(String),
    }
    Ok(match Option::<Flag>::deserialize(d)? {
        Some(Flag::Int(i)) => i,
        Some(Flag::Bool(b)) => i64::from(b),
        Some(Flag::Str(s)) => s.trim().parse().unwrap_or(0),
        None => 0,
    })
}

pub async fn fetch_lineup(http: &reqwest::Client, dev: &Device) -> anyhow::Result<Vec<Channel>> {
    let url = if !dev.lineup_url.trim().is_empty() {
        dev.lineup_url.trim().to_string()
    } else {
        let base = normalize_base_url(&dev.base_url);
        if base.is_empty() {
            return Err(anyhow!("device has neither LineupURL nor BaseURL"));
        }
        format!("{base}/lineup.json")
    };
    let body = get_limited(http, &url, LINEUP_JSON_LIMIT).await?;
    let entries: Vec<LineupEntry> = serde_json::from_slice(&body).with_context(|| format!("parse {url}"))?;
    let mut chans: Vec<Channel> = entries
        .into_iter()
        .filter(|e| !e.guide_number.trim().is_empty())
        .map(|e| Channel {
            guide_number: e.guide_number.trim().to_string(),
            guide_name: if e.guide_name.trim().is_empty() {
                e.guide_number.trim().to_string()
            } else {
                e.guide_name.trim().to_string()
            },
            url: e.url.trim().to_string(),
            hd: e.hd == 1,
            icon: None,
        })
        .collect();
    chans.sort_by(|a, b| {
        channel_sort_key(&a.guide_number)
            .partial_cmp(&channel_sort_key(&b.guide_number))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    chans.dedup_by(|a, b| a.guide_number == b.guide_number);
    Ok(chans)
}

/// Numeric sort for guide numbers like "7.1", "10.2", "1001".
pub fn channel_sort_key(gn: &str) -> (f64, String) {
    let mut parts = gn.splitn(2, '.');
    let major: f64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(f64::MAX);
    let minor: f64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0.0);
    (major + minor / 1000.0, gn.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};

    fn fake_reply(device_id: u32, base: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        push_tlv(&mut payload, TAG_DEVICE_TYPE, &0x0000_0001u32.to_be_bytes());
        push_tlv(&mut payload, TAG_DEVICE_ID, &device_id.to_be_bytes());
        push_tlv(&mut payload, TAG_BASE_URL, base.as_bytes());
        frame_packet(TYPE_DISCOVER_RPY, &payload)
    }

    #[test]
    fn discover_packet_wire_format() {
        let p = discover_packet();
        // header: type 0x0002, length 12
        assert_eq!(&p[..4], &[0x00, 0x02, 0x00, 0x0C]);
        assert_eq!(&p[4..10], &[0x01, 0x04, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&p[10..16], &[0x02, 0x04, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(p.len(), 20);
        let crc = crc32fast::hash(&p[..16]);
        assert_eq!(&p[16..], &crc.to_le_bytes());
        // A request is not a reply.
        assert_eq!(parse_reply(&p), None);
    }

    #[test]
    fn reply_round_trip() {
        let pkt = fake_reply(0x1053_ABCD, "http://192.168.1.20:80");
        assert_eq!(parse_reply(&pkt), Some(("1053ABCD".into(), "http://192.168.1.20:80".into())));
        // NUL-terminated base url is tolerated
        let pkt = fake_reply(1, "http://10.0.0.5\0");
        assert_eq!(parse_reply(&pkt), Some(("00000001".into(), "http://10.0.0.5".into())));
    }

    #[test]
    fn reply_corrupt_crc_rejected() {
        let mut pkt = fake_reply(0x1234_5678, "http://192.168.1.20:80");
        let last = pkt.len() - 1;
        pkt[last] ^= 0xFF;
        assert_eq!(parse_reply(&pkt), None);
        // corrupt payload byte too
        let mut pkt = fake_reply(0x1234_5678, "http://192.168.1.20:80");
        pkt[6] ^= 0x01;
        assert_eq!(parse_reply(&pkt), None);
    }

    #[test]
    fn reply_wrong_type_rejected() {
        let mut payload = Vec::new();
        push_tlv(&mut payload, TAG_DEVICE_ID, &1u32.to_be_bytes());
        push_tlv(&mut payload, TAG_BASE_URL, b"http://1.2.3.4");
        let pkt = frame_packet(TYPE_DISCOVER_REQ, &payload);
        assert_eq!(parse_reply(&pkt), None);
        let pkt = frame_packet(0x0004, &payload);
        assert_eq!(parse_reply(&pkt), None);
    }

    #[test]
    fn reply_truncated_or_missing_base() {
        assert_eq!(parse_reply(&[]), None);
        assert_eq!(parse_reply(&[0, 3, 0, 0]), None);
        let pkt = fake_reply(7, "http://x");
        assert_eq!(parse_reply(&pkt[..pkt.len() - 2]), None);
        // length claims more than present
        let mut pkt = fake_reply(7, "http://x");
        pkt[3] = 0xFF;
        assert_eq!(parse_reply(&pkt), None);
        // no base url tag → None
        let mut payload = Vec::new();
        push_tlv(&mut payload, TAG_DEVICE_ID, &1u32.to_be_bytes());
        assert_eq!(parse_reply(&frame_packet(TYPE_DISCOVER_RPY, &payload)), None);
    }

    #[test]
    fn long_tlv_two_byte_length() {
        let base: String = format!("http://{}.example/", "a".repeat(150));
        let pkt = fake_reply(9, &base);
        let (_, b) = parse_reply(&pkt).unwrap();
        assert_eq!(b, base.trim());
        let payload = &pkt[4..pkt.len() - 4];
        let items: Vec<_> = tlvs(payload).collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[2].0, TAG_BASE_URL);
        assert_eq!(items[2].1.len(), base.len());
    }

    #[test]
    fn sort_key_order() {
        let mut v = vec!["1001", "10.1", "2.10", "2.1", "abc", "7.1"];
        v.sort_by(|a, b| channel_sort_key(a).partial_cmp(&channel_sort_key(b)).unwrap());
        assert_eq!(v, vec!["2.1", "2.10", "7.1", "10.1", "1001", "abc"]);
        assert!(channel_sort_key("2.1") < channel_sort_key("2.10"));
        assert!(channel_sort_key("2.10") < channel_sort_key("10.1"));
        assert!(channel_sort_key("10.1") < channel_sort_key("1001"));
    }

    #[test]
    fn base_url_normalization() {
        assert_eq!(normalize_base_url("192.168.1.20"), "http://192.168.1.20");
        assert_eq!(normalize_base_url(" http://192.168.1.20:80/ "), "http://192.168.1.20:80");
        assert_eq!(normalize_base_url("https://tuner.local//"), "https://tuner.local");
        assert_eq!(normalize_base_url(""), "");
    }

    #[test]
    fn guide_number_validation() {
        assert!(is_valid_guide_number("7.1"));
        assert!(is_valid_guide_number("1001"));
        assert!(!is_valid_guide_number(""));
        assert!(!is_valid_guide_number("7.1/../x"));
        assert!(!is_valid_guide_number("7 1"));
    }

    #[test]
    fn lineup_hd_flag_variants() {
        let body = r#"[{"GuideNumber":"7.1","GuideName":"KABC","URL":"http://t/auto/v7.1","HD":1},
                       {"GuideNumber":"7.2","GuideName":"X","URL":"u","HD":true},
                       {"GuideNumber":"7.3","GuideName":"Y","URL":"u","HD":"1"},
                       {"GuideNumber":"7.4","GuideName":"Z","URL":"u"}]"#;
        let e: Vec<LineupEntry> = serde_json::from_str(body).unwrap();
        assert_eq!(e.iter().map(|e| e.hd).collect::<Vec<_>>(), vec![1, 1, 1, 0]);
    }

    async fn spawn_fake_tuner(lineup_url_in_discover: bool) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let base2 = base.clone();
        let app = Router::new()
            .route(
                "/discover.json",
                get(move || {
                    let base = base2.clone();
                    async move {
                        let mut v = serde_json::json!({
                            "FriendlyName": "HDHomeRun FLEX 4K",
                            "ModelNumber": "HDHR5-4K",
                            "FirmwareVersion": "20240101",
                            "DeviceID": "1053ABCD",
                            "TunerCount": 4,
                        });
                        if lineup_url_in_discover {
                            v["LineupURL"] = serde_json::Value::String(format!("{base}/alt/lineup.json"));
                            v["BaseURL"] = serde_json::Value::String(base.clone());
                        }
                        Json(v)
                    }
                }),
            )
            .route(
                "/lineup.json",
                get(|| async {
                    Json(serde_json::json!([
                        {"GuideNumber":"10.1","GuideName":"KTLA","URL":"http://t/auto/v10.1","HD":1},
                        {"GuideNumber":"2.1","GuideName":"KCBS","URL":"http://t/auto/v2.1","HD":1},
                        {"GuideNumber":"2.10","GuideName":"KCBS-SD","URL":"http://t/auto/v2.10","HD":0},
                        {"GuideNumber":"1001","GuideName":"WEATHER","URL":"http://t/auto/v1001"}
                    ]))
                }),
            )
            .route(
                "/alt/lineup.json",
                get(|| async {
                    Json(serde_json::json!([{"GuideNumber":"5.1","GuideName":"ALT","URL":"http://t/auto/v5.1","HD":1}]))
                }),
            );
        let h = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, h)
    }

    #[tokio::test]
    async fn fetch_discover_and_lineup_over_http() {
        let (base, h) = spawn_fake_tuner(false).await;
        let http = reqwest::Client::new();
        // bare host:port (no scheme) must be accepted
        let bare = base.trim_start_matches("http://");
        let dev = fetch_discover(&http, bare).await.unwrap();
        assert_eq!(dev.device_id, "1053ABCD");
        assert_eq!(dev.base_url, base, "base_url filled in when missing");
        assert_eq!(dev.tuner_count, 4);
        assert_eq!(dev.local_ip, "127.0.0.1");
        assert!(dev.lineup_url.is_empty());

        let chans = fetch_lineup(&http, &dev).await.unwrap();
        let nums: Vec<_> = chans.iter().map(|c| c.guide_number.as_str()).collect();
        assert_eq!(nums, vec!["2.1", "2.10", "10.1", "1001"]);
        assert!(chans[0].hd);
        assert!(!chans[1].hd);
        assert!(!chans[3].hd);
        assert_eq!(chans[2].guide_name, "KTLA");
        assert_eq!(chans[2].url, "http://t/auto/v10.1");
        assert!(chans.iter().all(|c| c.icon.is_none()));

        // 404 → error, not panic
        let bad = Device { base_url: format!("{base}/nope"), ..Device::default() };
        assert!(fetch_lineup(&http, &bad).await.is_err());
        assert!(fetch_discover(&http, &format!("{base}/nope")).await.is_err());
        h.abort();
    }

    #[tokio::test]
    async fn fetch_lineup_prefers_lineup_url() {
        let (base, h) = spawn_fake_tuner(true).await;
        let http = reqwest::Client::new();
        let dev = fetch_discover(&http, &format!("{base}/")).await.unwrap();
        assert_eq!(dev.lineup_url, format!("{base}/alt/lineup.json"));
        let chans = fetch_lineup(&http, &dev).await.unwrap();
        assert_eq!(chans.len(), 1);
        assert_eq!(chans[0].guide_number, "5.1");
        h.abort();
    }

    #[tokio::test]
    async fn discover_udp_parses_fake_device() {
        // Talk to a fake responder bound to the real discovery port when it is
        // free; otherwise just assert that the broadcast path does not fail.
        let Ok(responder) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVER_PORT)).await else {
            eprintln!("skipping: port {DISCOVER_PORT} busy");
            return;
        };
        let (base, h) = spawn_fake_tuner(false).await;
        let reply = fake_reply(0x1053_ABCD, &base);
        let responder_task = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            // answer every request we get until aborted
            loop {
                let Ok((n, from)) = responder.recv_from(&mut buf).await else {
                    return;
                };
                if &buf[..n] == discover_packet().as_slice() {
                    let _ = responder.send_to(&reply, from).await;
                }
            }
        });
        let http = reqwest::Client::new();
        let r = discover_udp(&http, Duration::from_millis(1500)).await;
        responder_task.abort();
        h.abort();
        match r {
            Ok(devs) => {
                // Broadcast may be filtered on some hosts; when a reply did arrive it must be right.
                if let Some(d) = devs.iter().find(|d| d.base_url == base) {
                    assert_eq!(d.device_id, "1053ABCD");
                    assert_eq!(d.tuner_count, 4);
                    assert!(!d.local_ip.is_empty());
                }
            }
            // Hosts without a broadcast route (sandboxes, some CI runners)
            // fail the send itself; that is an environment limitation.
            Err(e) if format!("{e:#}").contains("send discover broadcast") => {
                eprintln!("skipping: broadcast unavailable: {e:#}");
            }
            Err(e) => panic!("discover_udp failed: {e:#}"),
        }
    }
}
