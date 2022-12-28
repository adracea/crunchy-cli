use crate::utils::context::Context;
use anyhow::{bail, Result};
use crunchyroll_rs::media::{Resolution, VariantData, VariantSegment};
use indicatif::{ProgressBar, ProgressFinish, ProgressStyle};
use log::{debug, LevelFilter};
use rsubs_lib::vtt;
use std::borrow::{Borrow, BorrowMut};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use tokio::task::JoinSet;

pub fn find_resolution(
    mut streaming_data: Vec<VariantData>,
    resolution: &Resolution,
) -> Option<VariantData> {
    streaming_data.sort_by(|a, b| a.resolution.width.cmp(&b.resolution.width).reverse());
    match resolution.height {
        u64::MAX => Some(streaming_data.into_iter().next().unwrap()),
        u64::MIN => Some(streaming_data.into_iter().last().unwrap()),
        _ => streaming_data
            .into_iter()
            .find(|v| resolution.height == u64::MAX || v.resolution.height == resolution.height),
    }
}

pub async fn download_cc(ctx: &Context, path: String, ccurl: String) -> Result<()> {
    let client = Arc::new(ctx.crunchy.client());
    let data = vtt::parse(client.get(ccurl).send().await?.text().await?).to_ass();
    data.to_file(path)?;
    Ok(())
}

pub async fn download_segments(
    ctx: &Context,
    writer: &mut impl Write,
    message: Option<String>,
    variant_data: VariantData,
) -> Result<()> {
    let segments = variant_data.segments().await?;
    let total_segments = segments.len();

    let client = Arc::new(ctx.crunchy.client());
    let count = Arc::new(Mutex::new(0));

    let progress = if log::max_level() == LevelFilter::Info {
        let estimated_file_size = (variant_data.bandwidth / 8)
            * segments
                .iter()
                .map(|s| s.length.unwrap_or_default().as_secs())
                .sum::<u64>();

        let progress = ProgressBar::new(estimated_file_size)
            .with_style(
                ProgressStyle::with_template(
                    ":: {msg}{bytes:>10} {bytes_per_sec:>12} [{wide_bar}] {percent:>3}%",
                )
                .unwrap()
                .progress_chars("##-"),
            )
            .with_message(message.map(|m| m + " ").unwrap_or_default())
            .with_finish(ProgressFinish::Abandon);
        Some(progress)
    } else {
        None
    };

    let cpus = num_cpus::get();
    let mut segs: Vec<Vec<VariantSegment>> = Vec::with_capacity(cpus);
    for _ in 0..cpus {
        segs.push(vec![])
    }
    for (i, segment) in segments.clone().into_iter().enumerate() {
        segs[i - ((i / cpus) * cpus)].push(segment);
    }

    let (sender, receiver) = mpsc::channel();

    let mut join_set: JoinSet<Result<()>> = JoinSet::new();
    for num in 0..cpus {
        let thread_client = client.clone();
        let thread_sender = sender.clone();
        let thread_segments = segs.remove(0);
        let thread_count = count.clone();
        join_set.spawn(async move {
            for (i, segment) in thread_segments.into_iter().enumerate() {
                let response_res = thread_client
                    .get(&segment.url)
                    .timeout(Duration::from_secs(30u64))
                    .send()
                    .await;
                let verfified_response = match response_res {
                    Ok(x) => x,
                    Err(y) => panic!("This is likely a netowrking error: {}", y),
                };
                let possible_error_in_response = verfified_response.bytes().await;
                let mut buf = if let Ok(r) = possible_error_in_response {
                    r.to_vec()
                } else {
                    debug!(
                        "Segment Failed to download: {}, retrying.",
                        num + (i * cpus)
                    );
                    let mut resp = thread_client
                        .get(&segment.url)
                        .timeout(Duration::from_secs(30u64))
                        .send()
                        .await
                        .unwrap()
                        .bytes()
                        .await;
                    if resp.is_err() {
                        let mut retry_ctr = 1;
                        loop {
                            debug!(
                                "Segment Failed to download: {}, retry {}.",
                                num + (i * cpus),
                                retry_ctr
                            );
                            resp = thread_client
                                .get(&segment.url)
                                .timeout(Duration::from_secs(30u64))
                                .send()
                                .await
                                .unwrap()
                                .bytes()
                                .await;
                            if resp.is_ok() {
                                break;
                            }
                            retry_ctr += 1;
                        }
                    }
                    resp.unwrap().to_vec()
                };
                *thread_amount.lock().unwrap() += buf.len();

                buf = VariantSegment::decrypt(buf.borrow_mut(), segment.key)?.to_vec();

                let mut c = thread_count.lock().unwrap();
                debug!(
                    "Downloaded and decrypted segment [{}/{} {:.2}%] {}",
                    num + (i * cpus),
                    total_segments,
                    ((*c + 1) as f64 / total_segments as f64) * 100f64,
                    segment.url
                );
                thread_sender.send((num + (i * cpus), buf)).unwrap();

                *c += 1;
            }

            Ok(())
        });
    }

    drop(sender);
    let mut data_pos = 0usize;
    let mut buf: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for (pos, bytes) in receiver.iter() {
        if let Some(p) = &progress {
            let progress_len = p.length().unwrap();
            let estimated_segment_len = (variant_data.bandwidth / 8)
                * segments
                    .get(pos)
                    .unwrap()
                    .length
                    .unwrap_or_default()
                    .as_secs();
            let bytes_len = bytes.len() as u64;

            p.set_length(progress_len - estimated_segment_len + bytes_len);
            p.inc(bytes_len)
        }

        if data_pos == pos {
            writer.write_all(bytes.borrow())?;
            data_pos += 1;
        } else {
            buf.insert(pos, bytes);
        }
        while let Some(b) = buf.remove(&data_pos) {
            writer.write_all(b.borrow())?;
            data_pos += 1;
        }
        debug!("Buf is {:?} TL is {:?}", buf.len(), *count.lock().unwrap());
    }

    while let Some(joined) = join_set.join_next().await {
        joined??
    }

    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FFmpegPreset {
    Nvidia,

    Av1,
    H265,
    H264,
}

impl ToString for FFmpegPreset {
    fn to_string(&self) -> String {
        match self {
            &FFmpegPreset::Nvidia => "nvidia",
            &FFmpegPreset::Av1 => "av1",
            &FFmpegPreset::H265 => "h265",
            &FFmpegPreset::H264 => "h264",
        }
        .to_string()
    }
}

impl FFmpegPreset {
    pub(crate) fn all() -> Vec<FFmpegPreset> {
        vec![
            FFmpegPreset::Nvidia,
            FFmpegPreset::Av1,
            FFmpegPreset::H265,
            FFmpegPreset::H264,
        ]
    }

    pub(crate) fn description(self) -> String {
        match self {
            FFmpegPreset::Nvidia => "If you're have a nvidia card, use hardware / gpu accelerated video processing if available",
            FFmpegPreset::Av1 => "Encode the video(s) with the av1 codec. Hardware acceleration is currently not possible with this",
            FFmpegPreset::H265 => "Encode the video(s) with the h265 codec",
            FFmpegPreset::H264 => "Encode the video(s) with the h264 codec"
        }.to_string()
    }

    pub(crate) fn parse(s: &str) -> Result<FFmpegPreset, String> {
        Ok(match s.to_lowercase().as_str() {
            "nvidia" => FFmpegPreset::Nvidia,
            "av1" => FFmpegPreset::Av1,
            "h265" | "h.265" | "hevc" => FFmpegPreset::H265,
            "h264" | "h.264" => FFmpegPreset::H264,
            _ => return Err(format!("'{}' is not a valid ffmpeg preset", s)),
        })
    }

    pub(crate) fn ffmpeg_presets(
        mut presets: Vec<FFmpegPreset>,
    ) -> Result<(Vec<String>, Vec<String>)> {
        fn preset_check_remove(presets: &mut Vec<FFmpegPreset>, preset: FFmpegPreset) -> bool {
            if let Some(i) = presets.iter().position(|p| p == &preset) {
                presets.remove(i);
                true
            } else {
                false
            }
        }

        let nvidia = preset_check_remove(&mut presets, FFmpegPreset::Nvidia);
        if presets.len() > 1 {
            bail!(
                "Can only use one video codec, {} found: {}",
                presets.len(),
                presets
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            )
        }

        let (mut input, mut output) = (vec![], vec![]);
        for preset in presets {
            if nvidia {
                match preset {
                    FFmpegPreset::Av1 => bail!("'nvidia' hardware acceleration preset is not available in combination with the 'av1' codec preset"),
                    FFmpegPreset::H265 => {
                        input.extend(["-hwaccel", "cuvid", "-c:v", "h264_cuvid"]);
                        output.extend(["-c:v", "hevc_nvenc"]);
                    }
                    FFmpegPreset::H264 => {
                        input.extend(["-hwaccel", "cuvid", "-c:v", "h264_cuvid"]);
                        output.extend(["-c:v", "h264_nvenc"]);
                    }
                    _ => ()
                }
            } else {
                match preset {
                    FFmpegPreset::Av1 => {
                        output.extend(["-c:v", "libaom-av1"]);
                    }
                    FFmpegPreset::H265 => {
                        output.extend(["-c:v", "libx265"]);
                    }
                    FFmpegPreset::H264 => {
                        output.extend(["-c:v", "libx264"]);
                    }
                    _ => (),
                }
            }
        }

        if input.is_empty() && output.is_empty() {
            output.extend(["-c", "copy"])
        } else {
            if output.is_empty() {
                output.extend(["-c", "copy"])
            } else {
                output.extend(["-c:a", "copy", "-c:s", "copy"])
            }
        }

        Ok((
            input.into_iter().map(|i| i.to_string()).collect(),
            output.into_iter().map(|o| o.to_string()).collect(),
        ))
    }
}
