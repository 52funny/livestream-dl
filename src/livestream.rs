use std::collections::HashMap;
use std::fmt::Display;
use std::hash::Hash;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::channel::mpsc;
use futures::StreamExt;
use log::{info, trace};
use m3u8_rs::{ByteRange, Playlist};
use reqwest::header::{self, HeaderMap};
use reqwest::{Client, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies, RetryTransientMiddleware};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};
use tokio::{fs, time};

use crate::cli::{DownloadOptions, NetworkOptions};
use crate::encryption::Encryption;
use crate::mux::remux;
use crate::utils::make_absolute_url;

#[derive(Debug)]
pub struct Livestream {
    streams: HashMap<Stream, Url>,
    client: ClientWithMiddleware,
    stopper: Stopper,
    network_options: NetworkOptions,
}

#[derive(Clone, Debug)]
pub struct Stopper(Arc<(Notify, Mutex<bool>)>);

/// Used to signal m3u8 fetcher task to quit
impl Stopper {
    fn new() -> Self {
        Self(Arc::new((Notify::new(), Mutex::new(false))))
    }

    /// Wait for stopper to be notified
    async fn wait(&self) {
        self.0 .0.notified().await;
    }

    /// Check if stopped
    pub async fn stopped(&self) -> bool {
        *self.0 .1.lock().await
    }

    /// Set to stopped and notify waiters
    pub async fn stop(&self) {
        *self.0 .1.lock().await = true;
        self.0 .0.notify_waiters();
    }
}

/// Type of stream
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Stream {
    Main,

    // Alternative media
    Video { name: String, lang: Option<String> },
    Audio { name: String, lang: Option<String> },
    Subtitle { name: String, lang: Option<String> },
}

/// Type of media segment
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Segment {
    Initialization {
        url: Url,
        byte_range: Option<HashableByteRange>,
    },
    Sequence {
        url: Url,
        byte_range: Option<HashableByteRange>,
        discon_seq: u64,
        seq: u64,
    },
}

#[derive(Clone, Eq, Debug)]
pub struct HashableByteRange(ByteRange);

impl Deref for HashableByteRange {
    type Target = ByteRange;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PartialEq for HashableByteRange {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Hash for HashableByteRange {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.length.hash(state);
        self.0.offset.hash(state);
    }
}

type SegmentIdData = (Stream, Segment, Vec<u8>);

impl Segment {
    /// URL of segment
    fn url(&self) -> &Url {
        match self {
            Self::Initialization { url: u, .. } => u,
            Self::Sequence { url: u, .. } => u,
        }
    }

    /// String identifier of segment
    fn id(&self) -> String {
        match self {
            Self::Initialization { .. } => "init".into(),
            Self::Sequence {
                discon_seq: d,
                seq: s,
                ..
            } => format!("d{:010}s{:010}", d, s),
        }
    }

    fn byte_range(&self) -> Option<String> {
        let range = match self {
            Self::Initialization {
                byte_range: None, ..
            } => return None,
            Self::Sequence {
                byte_range: None, ..
            } => return None,
            Self::Initialization {
                byte_range: Some(b),
                ..
            } => b,
            Self::Sequence {
                byte_range: Some(b),
                ..
            } => b,
        };

        let start = range.offset.unwrap_or(0);
        let end = start + range.length.saturating_sub(1);

        Some(format!("bytes={}-{}", start, end))
    }
}

impl Stream {
    /// File extension for stream
    fn extension(&self) -> String {
        match self {
            Self::Main => "ts".into(),
            Self::Video { .. } => "ts".into(),
            Self::Audio { .. } => "m4a".into(),
            Self::Subtitle { .. } => "vtt".into(),
        }
    }

    /// Name of stream if available
    pub fn name(&self) -> Option<String> {
        match self {
            Self::Main => None,
            Self::Video { name: n, .. } => Some(n.clone()),
            Self::Audio { name: n, .. } => Some(n.clone()),
            Self::Subtitle { name: n, .. } => Some(n.clone()),
        }
    }
}

impl Display for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Main => write!(f, "main"),
            Self::Video { name: n, .. } => write!(f, "video_{}", n),
            Self::Audio { name: n, .. } => write!(f, "audio_{}", n),
            Self::Subtitle { name: n, .. } => write!(f, "subtitle_{}", n),
        }
    }
}

impl Livestream {
    /// Create a new Livestream
    ///
    /// If a master playlist is given, choose the highest bitrate variant and download its stream
    /// and all of its alternative media streams
    pub async fn new(url: &Url, network_options: &NetworkOptions) -> Result<(Self, Stopper)> {
        // Create reqwest client
        let client = Client::builder()
            .timeout(Duration::from_secs(network_options.timeout))
            .build()?;
        let retry_policy = policies::ExponentialBackoff::builder()
            .retry_bounds(Duration::from_secs(1), Duration::from_secs(10))
            .backoff_exponent(2)
            .build_with_max_retries(network_options.max_retries);
        let client = ClientBuilder::new(client)
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();

        // Check if m3u8 is master or media
        let resp = client.get(url.clone()).send().await?;
        let final_url = resp.url().clone();
        let bytes = resp.bytes().await?;

        // Parse m3u8 playlist and add streams
        let mut streams = HashMap::new();
        match m3u8_rs::parse_playlist(&bytes) {
            Ok((_, Playlist::MasterPlaylist(p))) => {
                // Find best variant
                let max_stream = p
                    .variants
                    .into_iter()
                    .filter_map(|v| Some((v.bandwidth.parse::<u64>().ok()?, v)))
                    .max_by_key(|(x, _)| *x)
                    .ok_or_else(|| anyhow::anyhow!("No streams found"))?
                    .1;

                // Add main stream
                streams.insert(Stream::Main, make_absolute_url(url, &max_stream.uri)?);

                // Closure to find alternative media with matching group id and add them to streams
                let mut add_alternative =
                    |group, f: fn(String, Option<String>) -> Stream| -> Result<()> {
                        for a in p.alternatives.iter().filter(|a| a.group_id == group) {
                            if let Some(a_url) = &a.uri {
                                streams.insert(
                                    f(a.name.clone(), a.language.clone()),
                                    make_absolute_url(url, a_url)?,
                                );
                            }
                        }
                        Ok(())
                    };

                // Add audio streams
                if let Some(group) = max_stream.audio {
                    add_alternative(group, |n, l| Stream::Audio { name: n, lang: l })?;
                }

                // Add video streams
                if let Some(group) = max_stream.video {
                    add_alternative(group, |n, l| Stream::Video { name: n, lang: l })?;
                }

                // Add subtitle streams
                if let Some(group) = max_stream.subtitles {
                    add_alternative(group, |n, l| Stream::Subtitle { name: n, lang: l })?;
                }
            }
            Ok((_, Playlist::MediaPlaylist(_))) => {
                streams.insert(Stream::Main, final_url);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Error parsing m3u8 playlist: {}", e));
            }
        }

        let stopper = Stopper::new();

        Ok((
            Self {
                streams,
                client,
                stopper: stopper.clone(),
                network_options: network_options.clone(),
            },
            stopper,
        ))
    }

    /// Download the livestream to disk
    pub async fn download(&self, options: &DownloadOptions) -> Result<()> {
        // m3u8 reader task handles
        let mut handles = Vec::new();

        let rx = {
            // Create channel for m3u8 fetcher <-> segment downloader tasks
            let (tx, rx) = mpsc::unbounded();

            // Spawn m3u8 reader task
            for (stream, url) in &self.streams {
                let client = self.client.clone();
                let stopper = self.stopper.clone();
                let tx = tx.clone();
                let stream = stream.clone();
                let url = url.clone();
                handles.push(tokio::spawn(async move {
                    m3u8_fetcher(client, stopper, tx, stream, url).await
                }));
            }

            rx
        };

        // Create segments directory if needed
        let segments_directory = options.output.join("segments");
        fs::create_dir_all(&segments_directory).await?;

        // Save initializations for each stream
        let mut init_map = HashMap::new();

        // Save paths for each downloaded segment
        let mut downloaded_segments = HashMap::new();

        // Download segments
        let mut buffered = rx
            .map(|(stream, seg, encryption)| fetch_segment(&self.client, stream, seg, encryption))
            .buffered(self.network_options.max_concurrent_downloads);
        while let Some(x) = buffered.next().await {
            save_segment(
                x?,
                &mut init_map,
                &mut downloaded_segments,
                &segments_directory,
            )
            .await?;
        }

        if let Some(remux_path) = &options.remux {
            // Remux if necessary
            let remux_path = options.output.join(remux_path);
            remux(downloaded_segments, &options.output, remux_path).await?;
        }

        // Check join handles
        for handle in handles {
            handle.await??;
        }

        Ok(())
    }
}

/// Periodically fetch m3u8 media playlist and send new segments to download task
async fn m3u8_fetcher(
    client: ClientWithMiddleware,
    notify_stop: Stopper,
    tx: mpsc::UnboundedSender<(Stream, Segment, Encryption)>,
    stream: Stream,
    url: Url,
) -> Result<()> {
    let mut last_seg = None;
    let mut init_downloaded = false;

    loop {
        // Fetch playlist
        let now = time::Instant::now();
        let mut found_new_segments = false;
        trace!("Fetching {}", url.as_str());
        let bytes = client.get(url.clone()).send().await?.bytes().await?;
        let media_playlist = m3u8_rs::parse_media_playlist(&bytes)
            .map_err(|e| anyhow::anyhow!("{:?}", e))?
            .1;

        // Loop through media segments
        let mut discon_offset = 0;
        let mut encryption = Encryption::None;
        for (seq, segment) in (media_playlist.media_sequence..).zip(media_playlist.segments.iter())
        {
            // Calculate segment discontinuity
            if segment.discontinuity {
                discon_offset += 1;
            }
            let discon_seq = media_playlist.discontinuity_sequence + discon_offset;

            // Skip segment if already downloaded
            if let Some(s) = last_seg {
                if s >= (discon_seq, seq) {
                    continue;
                }
            }

            // Check encryption
            if let Some(key) = &segment.key {
                encryption = Encryption::new(&client, key, &url, seq).await?;
            }

            // Segment is new
            last_seg = Some((discon_seq, seq));
            found_new_segments = true;

            // Download initialization if needed
            if !init_downloaded {
                if let Some(map) = &segment.map {
                    let init_url = make_absolute_url(&url, &map.uri)?;
                    trace!("Found new initialization segment {}", init_url.as_str());
                    if tx
                        .unbounded_send((
                            stream.clone(),
                            Segment::Initialization {
                                url: init_url,
                                byte_range: map
                                    .byte_range
                                    .as_ref()
                                    .map(|b| HashableByteRange(b.clone())),
                            },
                            Encryption::None,
                        ))
                        .is_err()
                    {
                        return Ok(());
                    }
                    init_downloaded = true;
                }
            }

            // Parse URL
            let seg_url = make_absolute_url(&url, &segment.uri)?;

            // Download segment
            trace!("Found new segment {}", seg_url.as_str());
            if tx
                .unbounded_send((
                    stream.clone(),
                    Segment::Sequence {
                        url: seg_url,
                        byte_range: segment
                            .byte_range
                            .as_ref()
                            .map(|b| HashableByteRange(b.clone())),
                        discon_seq,
                        seq,
                    },
                    encryption.clone(),
                ))
                .is_err()
            {
                return Ok(());
            }
        }

        // Return if stream ended
        if media_playlist.end_list {
            trace!("Playlist ended");
            return Ok(());
        }

        let wait_duration = if found_new_segments {
            // Wait for target duration if new segments were found
            Duration::from_secs_f32(media_playlist.target_duration)
        } else {
            // Otherwise wait for half target duration
            Duration::from_secs_f32(media_playlist.target_duration / 2.0)
        };

        // Wait until next interval or if stopped
        tokio::select! {
            biased;

            // Not cancel safe, but this is ok because all stoppers are notified when stopped, so
            // fairness doesn't matter
            _ = notify_stop.wait() => {},

            _ = time::sleep_until(now + wait_duration) => {},
        };

        // Return if stopped
        if notify_stop.stopped().await {
            return Ok(());
        }
    }
}

/// Download segment and save to disk if necessary
async fn fetch_segment(
    client: &ClientWithMiddleware,
    stream: Stream,
    segment: Segment,
    encryption: Encryption,
) -> Result<SegmentIdData> {
    let mut header_map = HeaderMap::new();
    let byte_range = segment.byte_range();
    if let Some(ref range) = byte_range {
        header_map.insert(header::RANGE, header::HeaderValue::from_str(range)?);
    }

    // Fetch segment
    let bytes: Vec<u8> = client
        .get(segment.url().clone())
        .headers(header_map)
        .send()
        .await?
        .bytes()
        .await?
        .into_iter()
        .collect();

    // Decrypt
    let bytes = encryption.decrypt(&bytes)?;

    info!(
        "Downloaded {} {}",
        segment.url().as_str(),
        byte_range.unwrap_or_else(|| "".into())
    );

    Ok((stream, segment, bytes))
}

async fn save_segment(
    (stream, segment, bytes): SegmentIdData,
    init_map: &mut HashMap<Stream, Vec<u8>>,
    downloaded_segments: &mut HashMap<Stream, Vec<(Segment, PathBuf)>>,
    segments_directory: impl AsRef<Path>,
) -> Result<()> {
    if matches!(segment, Segment::Initialization { .. }) {
        // If segment is initialization, save data for later use
        init_map.insert(stream.clone(), bytes);
    } else {
        // Save segment to disk
        let file_path = segments_directory.as_ref().join(format!(
            "segment_{}_{}.{}",
            stream,
            segment.id(),
            stream.extension()
        ));
        trace!("saving to {:?}", &file_path);
        let mut file = fs::File::create(&file_path).await?;

        // If initialization exists, write it first
        if let Some(init) = init_map.get(&stream) {
            file.write_all(init).await?;
        }

        // Write segment
        file.write_all(&bytes).await?;

        // Remember path
        downloaded_segments
            .entry(stream)
            .or_default()
            .push((segment, file_path));
    }

    Ok(())
}
