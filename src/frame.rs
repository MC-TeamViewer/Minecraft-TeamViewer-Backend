//! 门无关的流分帧 codec:`[varint LEB128 长度][payload]`。
//!
//! WebTransport 门与裸 QUIC 门共享此模块(alpha.5 起,替代旧 4 字节大端前缀);
//! 约定文档在 TeamViewRelay-Protocol 仓库,三端实现副本(Rust 此处 / TS 脚本 /
//! Java mod)不得漂移。datagram 通道不使用本模块——datagram 自带报文边界,
//! alpha.5 起一律无长度前缀。

use std::future::Future;
use std::io;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单帧 payload 上限。长度头即按此校验,声明超长直接断连(防缓冲无限增长)。
pub(crate) const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// 长度头最长 4 字节:LEB128 每 7bit 一组,8MiB(2^23)恰好 4 组封顶。
pub(crate) const MAX_FRAME_HEADER_LEN: usize = 4;

/// 对缓冲区做一次分帧扫描。
#[derive(Debug)]
pub(crate) enum FrameScan {
    /// 头部完整且 payload 已到齐,占 `header_len + payload_len` 字节。
    Complete {
        header_len: usize,
        payload_len: usize,
    },
    /// 合法前缀,还需更多字节(头部或 payload 未到齐)。
    Incomplete,
    /// 协议违规:varint 超长或声明的 payload 超过 [`MAX_FRAME_LEN`],必须断连。
    Malformed,
}

/// 从缓冲区头部解析一帧。只读入参,不消费;调用方确认 [`FrameScan::Complete`]
/// 后自行 advance。
pub(crate) fn split_frame_header(buffer: &[u8]) -> FrameScan {
    let mut value: usize = 0;
    for (index, byte) in buffer.iter().enumerate() {
        if index >= MAX_FRAME_HEADER_LEN {
            // 合法上限内 varint 至多 4 组;第 5 组必然超出 MAX_FRAME_LEN
            return FrameScan::Malformed;
        }
        value |= usize::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            if value > MAX_FRAME_LEN {
                return FrameScan::Malformed;
            }
            let header_len = index + 1;
            return if buffer.len() >= header_len + value {
                FrameScan::Complete {
                    header_len,
                    payload_len: value,
                }
            } else {
                FrameScan::Incomplete
            };
        }
    }
    FrameScan::Incomplete
}

/// 编码 varint(LEB128,低 7bit 组在前,最高位为续传标志),返回占用字节数。
pub(crate) fn encode_varint(value: usize, out: &mut [u8; MAX_FRAME_HEADER_LEN]) -> usize {
    debug_assert!(value <= MAX_FRAME_LEN);
    let mut rest = value;
    let mut len = 0;
    for slot in out.iter_mut() {
        let byte = (rest & 0x7f) as u8;
        rest >>= 7;
        len += 1;
        if rest == 0 {
            *slot = byte;
            return len;
        }
        *slot = byte | 0x80;
    }
    unreachable!("value > MAX_FRAME_LEN 应在调用前拒绝");
}

/// 从流上循环读取 `[varint 长度][payload]` 帧,逐帧回调。
///
/// 回调返回 `false` 表示调用方要求停止(会话结束),流被正常关闭。
/// 流正常结束(对端 fin)且缓冲无残留完整帧时返回 `Ok(())`;
/// 违反长度上限时返回错误,由调用方断连。
pub(crate) async fn read_frames<S, F, Fut>(stream: &mut S, mut on_frame: F) -> io::Result<()>
where
    S: AsyncRead + Unpin,
    F: FnMut(Bytes) -> Fut,
    Fut: Future<Output = bool>,
{
    let mut buffer = BytesMut::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match split_frame_header(&buffer) {
            FrameScan::Complete {
                header_len,
                payload_len,
            } => {
                let frame_len = header_len + payload_len;
                let frame = buffer[header_len..frame_len].to_vec();
                buffer.advance(frame_len);
                if !on_frame(Bytes::from(frame)).await {
                    return Ok(());
                }
                continue;
            }
            FrameScan::Incomplete => {}
            FrameScan::Malformed => {
                return Err(io::Error::other("stream frame exceeds length limit"));
            }
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            // 对端正常关闭;残留的不完整帧随缓冲丢弃
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// 向流写入一帧:`[varint 长度][payload]`。
pub(crate) async fn write_frame<S>(stream: &mut S, payload: &[u8]) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_LEN {
        return Err(io::Error::other("frame payload exceeds length limit"));
    }
    let mut header = [0u8; MAX_FRAME_HEADER_LEN];
    let header_len = encode_varint(payload.len(), &mut header);
    stream.write_all(&header[..header_len]).await?;
    stream.write_all(payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn encoded_varints(values: &[usize]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut header = [0u8; MAX_FRAME_HEADER_LEN];
        for value in values {
            let len = encode_varint(*value, &mut header);
            out.extend_from_slice(&header[..len]);
        }
        out
    }

    #[test]
    fn varint_roundtrip_covers_bit_boundaries() {
        let values = [
            0usize,
            1,
            127,
            128,
            16383,
            16384,
            2097151,
            2097152,
            MAX_FRAME_LEN,
        ];
        let bytes = encoded_varints(&values);
        let mut rest = &bytes[..];
        for value in values {
            // 手工解码 LEB128 直到续传位清零
            let mut decoded = 0usize;
            let mut consumed = 0;
            for (index, byte) in rest.iter().enumerate() {
                decoded |= usize::from(byte & 0x7f) << (7 * index);
                consumed = index + 1;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            assert_eq!(decoded, value);
            rest = &rest[consumed..];
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn varint_lengths_match_leb128_widths() {
        let cases = [
            (0usize, 1),
            (127, 1),
            (128, 2),
            (16383, 2),
            (16384, 3),
            (MAX_FRAME_LEN, 4),
        ];
        for (value, width) in cases {
            let mut header = [0u8; MAX_FRAME_HEADER_LEN];
            assert_eq!(encode_varint(value, &mut header), width, "value {value}");
        }
    }

    #[test]
    fn splits_complete_frame_from_buffer() {
        let mut buffer = encoded_varints(&[7]);
        buffer.extend_from_slice(b"web-map");
        match split_frame_header(&buffer) {
            FrameScan::Complete {
                header_len,
                payload_len,
            } => {
                assert_eq!(header_len, 1);
                assert_eq!(payload_len, 7);
                assert_eq!(&buffer[header_len..header_len + payload_len], b"web-map");
            }
            other => panic!("expected complete frame, got {other:?}"),
        }
    }

    #[test]
    fn partial_header_and_partial_payload_are_incomplete() {
        // 128 需要两字节长度头,只到一字节
        assert!(matches!(split_frame_header(&[0x80]), FrameScan::Incomplete));
        // 头部完整、payload 只到一半
        let mut buffer = encoded_varints(&[16]);
        buffer.extend_from_slice(&[0u8; 8]);
        assert!(matches!(split_frame_header(&buffer), FrameScan::Incomplete));
        assert!(matches!(split_frame_header(&[]), FrameScan::Incomplete));
    }

    #[test]
    fn oversized_varint_and_length_are_malformed() {
        // 第 5 个续传字节:超出头部宽度上限
        assert!(matches!(
            split_frame_header(&[0x80, 0x80, 0x80, 0x80, 0x00]),
            FrameScan::Malformed
        ));
        // 4 字节 varint 编码 MAX_FRAME_LEN + 1
        assert!(matches!(
            split_frame_header(&[0x81, 0x80, 0x80, 0x04]),
            FrameScan::Malformed
        ));
    }

    /// 每次 poll 最多吐出 `limit` 字节的 reader,用于测试跨读缓冲的分帧重组。
    struct ChunkReader {
        data: Vec<u8>,
        offset: usize,
        limit: usize,
    }

    impl AsyncRead for ChunkReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let end = (self.offset + self.limit).min(self.data.len());
            let slice = &self.data[self.offset..end];
            buf.put_slice(slice);
            self.offset = end;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn read_frames_reassembles_chunks_and_stops_on_request() {
        let payloads: Vec<Vec<u8>> = vec![b"alpha".to_vec(), vec![7u8; 40_000], b"omega".to_vec()];
        let mut wire = Vec::new();
        for payload in &payloads {
            let mut header = [0u8; MAX_FRAME_HEADER_LEN];
            let len = encode_varint(payload.len(), &mut header);
            wire.extend_from_slice(&header[..len]);
            wire.extend_from_slice(payload);
        }
        // 3B 长度头的帧跨过多个 16KiB 读缓冲
        let mut reader = ChunkReader {
            data: wire.clone(),
            offset: 0,
            limit: 5_000,
        };
        let mut seen = Vec::new();
        read_frames(&mut reader, |frame| {
            seen.push(frame.to_vec());
            async move { true }
        })
        .await
        .expect("read to completion");
        assert_eq!(seen, payloads);

        // 回调返回 false:首帧后即停,流被正常关闭
        let mut reader = ChunkReader {
            data: wire,
            offset: 0,
            limit: 1,
        };
        let mut seen = 0;
        read_frames(&mut reader, |_frame| {
            seen += 1;
            async move { false }
        })
        .await
        .expect("stop cleanly");
        assert_eq!(seen, 1);
    }

    #[tokio::test]
    async fn write_frame_roundtrips_through_cursor() {
        let payload = vec![42u8; 70_000];
        let mut cursor = io::Cursor::new(Vec::new());
        write_frame(&mut cursor, &payload).await.expect("write");
        write_frame(&mut cursor, b"tiny").await.expect("write");
        cursor.set_position(0);

        let mut seen = Vec::new();
        read_frames(&mut cursor, |frame| {
            seen.push(frame.to_vec());
            async move { true }
        })
        .await
        .expect("read back");
        assert_eq!(seen, vec![payload, b"tiny".to_vec()]);
    }
}
