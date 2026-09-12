use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const PACKET_HEADER_SIZE: usize = 12;
const FLAG_SESSION: u64 = 1 << 63;
const FLAG_CONFIG: u64 = 1 << 62;
const FLAG_KEYFRAME: u64 = 1 << 61;
const PTS_MASK: u64 = FLAG_KEYFRAME - 1;
const MAX_PACKET: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    pub fn from_id(id: u32) -> Result<Self> {
        match id {
            0x68323634 => Ok(Self::H264),
            0x68323635 => Ok(Self::H265),
            0x00617631 => Ok(Self::Av1),
            _ => bail!("unsupported video codec id 0x{id:08x}"),
        }
    }
    pub fn needs_config_merge(self) -> bool {
        matches!(self, Self::H264 | Self::H265)
    }
}

#[derive(Debug)]
pub enum StreamPacket {
    Session {
        width: u32,
        height: u32,
        client_resized: bool,
    },
    Media {
        pts_us: Option<i64>,
        keyframe: bool,
        data: Vec<u8>,
    },
}

#[derive(Default)]
pub struct PacketReader {
    pending_config: Option<Vec<u8>>,
    /// Keep the most recent codec config so a decoder that was recreated after
    /// an idle gap can initialize from the next keyframe.
    last_config: Option<Vec<u8>>,
    /// Keep a complete keyframe as a resume anchor. A suspended session may
    /// discard all delta packets while the device encoder does not emit a new
    /// IDR for several seconds; replaying this bounded packet lets FFmpeg
    /// recover immediately when demand returns.
    last_keyframe: Option<(Arc<Vec<u8>>, i64)>,
}

impl PacketReader {
    pub fn take_last_keyframe(&mut self) -> Option<(Vec<u8>, i64)> {
        self.last_keyframe
            .take()
            .map(|(data, pts_us)| ((*data).clone(), pts_us))
    }

    pub fn last_keyframe(&self) -> Option<(Arc<Vec<u8>>, i64)> {
        self.last_keyframe.clone()
    }
}

impl PacketReader {
    pub async fn read<R: AsyncRead + Unpin>(
        &mut self,
        source: &mut R,
        codec: Codec,
    ) -> Result<StreamPacket> {
        loop {
            let mut header = [0u8; PACKET_HEADER_SIZE];
            source.read_exact(&mut header).await?;
            let flags = u64::from_be_bytes(header[..8].try_into().unwrap());
            if flags & FLAG_SESSION != 0 {
                // A resize/session-generation change invalidates the previous
                // codec configuration. The server will send a fresh config;
                // carrying the old SPS/VPS across generations can make a
                // resumed decoder accept frames with the wrong dimensions.
                self.pending_config = None;
                self.last_config = None;
                self.last_keyframe = None;
                return Ok(StreamPacket::Session {
                    width: u32::from_be_bytes(header[4..8].try_into().unwrap()),
                    height: u32::from_be_bytes(header[8..12].try_into().unwrap()),
                    client_resized: header[3] & 1 != 0,
                });
            }
            let len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
            if len == 0 || len > MAX_PACKET {
                bail!("invalid video packet length {len}");
            }
            let mut data = vec![0; len];
            source.read_exact(&mut data).await?;
            if flags & FLAG_CONFIG != 0 && codec.needs_config_merge() {
                self.last_config = Some(data.clone());
                self.pending_config = Some(data);
                continue;
            }
            if let Some(config) = self.pending_config.take() {
                let capacity = config
                    .len()
                    .checked_add(data.len())
                    .context("codec config packet is too large")?;
                let mut merged = Vec::with_capacity(capacity);
                merged.extend(config);
                merged.extend(data);
                data = merged;
            } else if flags & FLAG_KEYFRAME != 0 && codec.needs_config_merge() {
                if let Some(config) = self.last_config.as_ref() {
                    let capacity = config
                        .len()
                        .checked_add(data.len())
                        .context("codec config packet is too large")?;
                    let mut merged = Vec::with_capacity(capacity);
                    merged.extend_from_slice(config);
                    merged.extend(data);
                    data = merged;
                }
            }
            let keyframe = flags & FLAG_KEYFRAME != 0;
            let pts_us = (flags & PTS_MASK) as i64;
            if keyframe {
                self.last_keyframe = Some((Arc::new(data.clone()), pts_us));
            }
            return Ok(StreamPacket::Media {
                pts_us: Some(pts_us),
                keyframe,
                data,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn merges_h264_config_once() {
        let mut bytes = Vec::new();
        bytes.extend(FLAG_CONFIG.to_be_bytes());
        bytes.extend(2u32.to_be_bytes());
        bytes.extend([1, 2]);
        bytes.extend((FLAG_KEYFRAME | 42).to_be_bytes());
        bytes.extend(2u32.to_be_bytes());
        bytes.extend([3, 4]);
        let mut reader = PacketReader::default();
        let packet = reader
            .read(&mut bytes.as_slice(), Codec::H264)
            .await
            .unwrap();
        match packet {
            StreamPacket::Media {
                pts_us,
                keyframe,
                data,
            } => {
                assert_eq!(pts_us, Some(42));
                assert!(keyframe);
                assert_eq!(data, vec![1, 2, 3, 4])
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn prepends_cached_config_to_a_later_keyframe() {
        let mut bytes = Vec::new();
        bytes.extend(FLAG_CONFIG.to_be_bytes());
        bytes.extend(2u32.to_be_bytes());
        bytes.extend([1, 2]);
        bytes.extend((FLAG_KEYFRAME | 42).to_be_bytes());
        bytes.extend(2u32.to_be_bytes());
        bytes.extend([3, 4]);
        bytes.extend((FLAG_KEYFRAME | 43).to_be_bytes());
        bytes.extend(2u32.to_be_bytes());
        bytes.extend([5, 6]);
        let mut reader = PacketReader::default();
        let mut source = bytes.as_slice();
        let first = reader.read(&mut source, Codec::H264).await.unwrap();
        let second = reader.read(&mut source, Codec::H264).await.unwrap();
        let StreamPacket::Media {
            data: first_data, ..
        } = first
        else {
            panic!("expected first media packet")
        };
        let StreamPacket::Media {
            data: second_data, ..
        } = second
        else {
            panic!("expected second media packet")
        };
        assert_eq!(first_data, vec![1, 2, 3, 4]);
        assert_eq!(second_data, vec![1, 2, 5, 6]);
        assert_eq!(reader.take_last_keyframe(), Some((vec![1, 2, 5, 6], 43)));
    }
}
