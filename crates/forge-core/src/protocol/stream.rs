use anyhow::{bail, Result};
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
                self.pending_config = Some(data);
                continue;
            }
            if let Some(config) = self.pending_config.take() {
                let mut merged = Vec::with_capacity(config.len() + data.len());
                merged.extend(config);
                merged.extend(data);
                data = merged;
            }
            return Ok(StreamPacket::Media {
                pts_us: Some((flags & PTS_MASK) as i64),
                keyframe: flags & FLAG_KEYFRAME != 0,
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
}
