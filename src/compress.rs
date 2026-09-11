//! 压缩套协商与 zstd 流压缩(alpha.6)。
//!
//! **套(suite)** 是"传输门原生协商载体"上商定的压缩行为组合:
//! - `Plain`     ——流与 datagram 均不压缩;
//! - `Zstd`      ——可靠流走 zstd,datagram 走独立压缩;
//! - `ZstdDict`  ——可靠流同 Zstd,datagram 另有字典模式(后续切片接入)。
//!
//! 各门协商载体(0 额外 RTT,服务端确认制):裸 QUIC 用 ALPN
//! (`teamviewrelay/v1[+zstd[-dict]]`);WS 门用子协议
//! (`teamviewrelay.{plain,zstd,zstd-dict}.v1`);WT 门用 extended CONNECT 的
//! `WT-Available-Protocols`/`WT-Protocol` 头(协议值与 WS 子协议同名)。
//! 同一套语义在三门等价;WS 门无 datagram,其下 `ZstdDict` 与 `Zstd` 行为一致。
//!
//! **可靠流 zstd 语义 = 一条连续 zstd 流的分块切片**:发送端每连接一个持久
//! CCtx,逐 envelope `write + flush`(ZSTD_e_flush 保证即时可解码),flush
//! 产出的压缩字节作为一帧 payload 走 `[varint 长度][压缩块]`;接收端把逐帧
//! 压缩块持续喂进同一条持久 DCtx,读出的解压字节即 envelope 本体——压缩块
//! 边界与 envelope 边界一一对应,跨帧共享压缩上下文(等效 permessage-deflate
//! 的 context takeover)。跨连接无共享状态;datagram 通道不适用本模块
//! (自包含单元,字典模式另属 door-control 约定)。

use std::io;

use bytes::Bytes;

#[cfg(test)]
use crate::frame::MAX_FRAME_LEN;
use crate::quic_transport::{ALPN_PLAIN, ALPN_ZSTD, ALPN_ZSTD_DICT};

/// 下行流压缩级别的默认值(可用 `TEAMVIEWER_ZSTD_LEVEL` 覆盖,1..=22)。
/// 级别是纯发送端决策,zstd 帧自描述,无需协商。
pub(crate) const DEFAULT_STREAM_COMPRESSION_LEVEL: i32 = 3;

/// 解压窗口上限(2^23 = 8 MiB):防对端大窗口帧逼出大内存分配。
/// 仅测试用(服务端不再解压任何流,客户端实现须自行设同款上限)。
#[cfg(test)]
const MAX_DECOMPRESSION_WINDOW_LOG: u32 = 23;

/// 门协商出的压缩套。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Suite {
    #[default]
    Plain,
    Zstd,
    ZstdDict,
}

impl Suite {
    /// 按 QUIC 门协商出的 ALPN 识别套;未知值返回 None(rustls 已先拒,
    /// 此处兜底防御)。
    pub(crate) fn from_alpn(alpn: &str) -> Option<Suite> {
        match alpn {
            ALPN_PLAIN => Some(Suite::Plain),
            ALPN_ZSTD => Some(Suite::Zstd),
            ALPN_ZSTD_DICT => Some(Suite::ZstdDict),
            _ => None,
        }
    }

    /// 可靠流下行(服务端→客户端)是否走 zstd 分块压缩。压缩套语义为
    /// 单向:上行恒为 plain 分帧(上行载荷小,且浏览器端 fzstd 仅解压)。
    /// WS 门无 datagram,其 `ZstdDict` 与 `Zstd` 在流上行为一致,故仅按
    /// 此判断即可。
    pub(crate) fn stream_zstd(self) -> bool {
        matches!(self, Suite::Zstd | Suite::ZstdDict)
    }

    /// datagram 通道是否走 zstd(QUIC/WT 门):`+zstd` 套单帧独立压缩,
    /// `+zstd-dict` 套字典模式;`plain` 套原样裸 envelope。
    pub(crate) fn datagram_zstd(self) -> bool {
        !matches!(self, Suite::Plain)
    }
}

/// WS 门子协议(压缩套协商载体):`teamviewrelay.{plain,zstd,zstd-dict}.v1`。
/// 客户端按偏好序放在 `Sec-WebSocket-Protocol`,服务端择一并回显(确认制,
/// 0 额外 RTT);不回显即未选定,客户端按自身回退链降级(deflate/明文)。
pub(crate) const SUBPROTOCOL_PLAIN: &str = "teamviewrelay.plain.v1";
pub(crate) const SUBPROTOCOL_ZSTD: &str = "teamviewrelay.zstd.v1";
pub(crate) const SUBPROTOCOL_ZSTD_DICT: &str = "teamviewrelay.zstd-dict.v1";

impl Suite {
    /// 按 WS 子协议识别套;非 teamviewrelay 子协议返回 None(维持既有
    /// permessage-deflate 协商路径,不得误判)。
    pub(crate) fn from_subprotocol(value: &str) -> Option<Suite> {
        match value {
            SUBPROTOCOL_PLAIN => Some(Suite::Plain),
            SUBPROTOCOL_ZSTD => Some(Suite::Zstd),
            SUBPROTOCOL_ZSTD_DICT => Some(Suite::ZstdDict),
            _ => None,
        }
    }

    /// 服务端确认制:按客户端偏好序取第一个可识别的 teamviewrelay 子协议。
    /// 客户端未提供任何 teamviewrelay 子协议返回 None(旧客户端,走原路径)。
    pub(crate) fn select_ws_subprotocol<'a, I>(offered: I) -> Option<Suite>
    where
        I: IntoIterator<Item = &'a str>,
    {
        offered.into_iter().find_map(Suite::from_subprotocol)
    }

    /// 选定套回显给 WS 客户端的子协议值。回显必须取自客户端实际提供的列表
    /// (RFC 6455 硬性要求,严格客户端会拒绝列表外的回显),因此 zstd-dict
    /// 原样回显、不折叠为 zstd——WS 门虽无 datagram,`ZstdDict` 的流行为与
    /// `Zstd` 一致,客户端按"收到 dict 回显即 zstd 流语义"解释即可。
    pub(crate) fn ws_subprotocol(self) -> &'static str {
        self.protocol_name()
    }

    /// 套的全名协议 token(zstd-dict 保留原名)。WS 与 WT 两门的回显统一
    /// 取原值(见 ws_subprotocol 的 RFC 6455 论据)。
    pub(crate) fn protocol_name(self) -> &'static str {
        match self {
            Suite::Plain => SUBPROTOCOL_PLAIN,
            Suite::Zstd => SUBPROTOCOL_ZSTD,
            Suite::ZstdDict => SUBPROTOCOL_ZSTD_DICT,
        }
    }

    /// 选定套回显给 WT 客户端的 `WT-Protocol` 头值。
    ///
    /// 必须是**裸 token**(不带 RFC 9651 List 的引号):带引号回显使
    /// Chrome 146/151 立即 "Opening handshake failed" 中止会话建立
    /// (2026-09-11 生产实验实证,见 42c47f9)。但裸 token 也只是「不中止」:
    /// 同日抓包实证 Chromium 并不把该头暴露到 `session.protocol`(回执
    /// 在线、会话建立,脚本仍读到空串),浏览器脚本的套协商因此走 URL
    /// query(`?suite=<plain|zstd|zstd-dict>`,见 web_transport.rs)。回显
    /// 保留仅作未来浏览器实现协商语义后的前向兼容。
    pub(crate) fn wt_protocol_value(self) -> String {
        self.protocol_name().to_owned()
    }

    /// WT 门 URL query `suite=` 参数值 → 套。短名三选一(`plain` / `zstd` /
    /// `zstd-dict`),大小写不敏感;其余任何值(含空串)一律 `ZstdDict`——
    /// query 是 WT 门套协商的唯一权威来源,默认取压缩率最高的字典套,
    /// 打错字的客户端与不声明能力的客户端行为一致,无需区分。
    pub(crate) fn from_query_value(value: &str) -> Suite {
        match value.trim().to_ascii_lowercase().as_str() {
            "plain" => Suite::Plain,
            "zstd" => Suite::Zstd,
            _ => Suite::ZstdDict,
        }
    }
}

/// 下行(服务端→客户端)持久压缩器:逐 envelope flush 出自包含可即时解码
/// 的压缩块。每连接实例,仅由单个写出任务使用。
pub(crate) struct StreamEncoder {
    encoder: zstd::stream::Encoder<'static, Vec<u8>>,
}

impl StreamEncoder {
    pub(crate) fn new(compression_level: i32) -> io::Result<StreamEncoder> {
        Ok(StreamEncoder {
            encoder: zstd::stream::Encoder::new(Vec::new(), compression_level)?,
        })
    }

    /// 压缩一个 envelope,返回作为一帧 payload 的压缩块(永不为空——
    /// flush 强制产出)。
    pub(crate) fn compress_chunk(&mut self, envelope: &[u8]) -> io::Result<Bytes> {
        use std::io::Write;
        self.encoder.write_all(envelope)?;
        // ZSTD_e_flush:把内部缓冲推成完整可解码的块序列,但不结束流,
        // 跨块共享压缩上下文
        self.encoder.flush()?;
        let sink = self.encoder.get_mut();
        Ok(Bytes::from(std::mem::take(sink)))
    }
}

/// 下行(服务端→客户端)流持久解压器:与对端 StreamEncoder 配对的同一连续
/// zstd 流。服务端不解压任何上行(压缩套语义为单向,上行恒 plain);本结构
/// 用于测试与服务端出站的解码验证,也是客户端实现(fzstd/zstd-jni)对齐的
/// 参照。每连接实例,仅由单个读任务使用。
#[cfg(test)]
pub(crate) struct StreamDecoder {
    dctx: zstd_safe::DCtx<'static>,
}

#[cfg(test)]
impl StreamDecoder {
    pub(crate) fn new() -> io::Result<StreamDecoder> {
        let mut dctx = zstd_safe::DCtx::create();
        dctx.set_parameter(zstd_safe::DParameter::WindowLogMax(
            MAX_DECOMPRESSION_WINDOW_LOG,
        ))
        .map_err(zstd_error)?;
        Ok(StreamDecoder { dctx })
    }

    /// 解压一个压缩块,返回 envelope 本体。分块边界由发送端 flush 保证与
    /// envelope 对齐,但本实现不依赖该假设:输出按流推进,多块拼接同样
    /// 正确。解压结果超过帧上限视为协议违规。
    pub(crate) fn decompress_chunk(&mut self, chunk: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut input_pos = 0usize;
        let mut window = [0u8; 64 * 1024];
        loop {
            let mut in_buffer = zstd_safe::InBuffer::around(&chunk[input_pos..]);
            let mut out_buffer = zstd_safe::OutBuffer::around(&mut window);
            zstd_safe::DCtx::decompress_stream(&mut self.dctx, &mut out_buffer, &mut in_buffer)
                .map_err(zstd_error)?;
            input_pos += in_buffer.pos();
            let produced = out_buffer.as_slice().len();
            out.extend_from_slice(out_buffer.as_slice());
            if out.len() > MAX_FRAME_LEN {
                return Err(io::Error::other(
                    "zstd stream decompressed past 8 MiB frame limit",
                ));
            }
            if input_pos >= chunk.len() && produced == 0 {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
fn zstd_error(error: zstd_safe::ErrorCode) -> io::Error {
    io::Error::other(format!("zstd error: {error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_envelopes() -> Vec<Vec<u8>> {
        vec![
            vec![0x08, 0x01],
            vec![7u8; 4096],
            (0..100_000u32).map(|v| (v % 251) as u8).collect(),
            b"tiny".to_vec(),
        ]
    }

    /// 逐 envelope 压缩→按任意边界切开喂解码器→按序还原。
    #[test]
    fn round_trip_across_arbitrary_chunk_boundaries() {
        let mut encoder = StreamEncoder::new(DEFAULT_STREAM_COMPRESSION_LEVEL).expect("encoder");
        let mut decoder = StreamDecoder::new().expect("decoder");
        let mut compressed = Vec::new();
        let mut expected = Vec::new();
        for envelope in sample_envelopes() {
            let chunk = encoder.compress_chunk(&envelope).expect("compress");
            assert!(!chunk.is_empty(), "flush must produce bytes");
            compressed.push(chunk);
            expected.push(envelope);
        }
        // 把全部压缩块拼起来,按 997 字节(质数)切开发给解码器
        let all: Vec<u8> = compressed.concat();
        let mut restored = Vec::new();
        for piece in all.chunks(997) {
            restored.extend_from_slice(&decoder.decompress_chunk(piece).expect("decompress"));
        }
        assert_eq!(restored, expected.concat());
    }

    /// 逐块解码(发送端 flush 对齐语义):每块解出恰好一个 envelope。
    #[test]
    fn per_chunk_decode_yields_envelopes_in_order() {
        let mut encoder = StreamEncoder::new(DEFAULT_STREAM_COMPRESSION_LEVEL).expect("encoder");
        let mut decoder = StreamDecoder::new().expect("decoder");
        for envelope in sample_envelopes() {
            let chunk = encoder.compress_chunk(&envelope).expect("compress");
            let decoded = decoder.decompress_chunk(&chunk).expect("decompress");
            assert_eq!(decoded, envelope);
        }
    }

    /// 非法压缩块必须显式报错,不得静默产出错误字节。
    #[test]
    fn garbage_chunk_is_rejected() {
        let mut decoder = StreamDecoder::new().expect("decoder");
        let garbage = [0xffu8; 64];
        assert!(decoder.decompress_chunk(&garbage).is_err());
    }

    #[test]
    fn alpn_mapping_covers_all_suites() {
        assert_eq!(Suite::from_alpn(ALPN_PLAIN), Some(Suite::Plain));
        assert_eq!(Suite::from_alpn(ALPN_ZSTD), Some(Suite::Zstd));
        assert_eq!(Suite::from_alpn(ALPN_ZSTD_DICT), Some(Suite::ZstdDict));
        assert_eq!(Suite::from_alpn("h3"), None);
        assert!(Suite::ZstdDict.stream_zstd() && Suite::Zstd.stream_zstd());
        assert!(!Suite::Plain.stream_zstd());
    }

    #[test]
    fn ws_subprotocol_selection_follows_client_preference() {
        // 按客户端偏好序取第一个可识别值
        assert_eq!(
            Suite::select_ws_subprotocol([SUBPROTOCOL_ZSTD, SUBPROTOCOL_PLAIN, "chat"]),
            Some(Suite::Zstd)
        );
        assert_eq!(
            Suite::select_ws_subprotocol([SUBPROTOCOL_PLAIN, SUBPROTOCOL_ZSTD]),
            Some(Suite::Plain)
        );
        assert_eq!(
            Suite::select_ws_subprotocol([SUBPROTOCOL_ZSTD_DICT]),
            Some(Suite::ZstdDict)
        );
        // 旧客户端:无 teamviewrelay 子协议 → None(维持 permessage-deflate 路径)
        assert_eq!(Suite::select_ws_subprotocol(["chat", "game.v2"]), None);
        assert_eq!(Suite::select_ws_subprotocol([]), None);
        // WS 门回显取原值:RFC 6455 要求回显 ∈ 客户端提供的列表,
        // 严格客户端(websockets 库实测)会拒绝列表外的折叠回显
        assert_eq!(Suite::ZstdDict.ws_subprotocol(), SUBPROTOCOL_ZSTD_DICT);
        assert_eq!(Suite::Plain.ws_subprotocol(), SUBPROTOCOL_PLAIN);
        assert_eq!(Suite::Zstd.ws_subprotocol(), SUBPROTOCOL_ZSTD);
        assert_eq!(
            Suite::from_subprotocol("teamviewrelay.zstd.v1"),
            Some(Suite::Zstd)
        );
        assert_eq!(Suite::from_subprotocol(""), None);
    }

    #[test]
    fn wt_query_value_selects_suite_with_zstd_dict_default() {
        // 短名三选一,大小写不敏感、容忍空白
        assert_eq!(Suite::from_query_value("plain"), Suite::Plain);
        assert_eq!(Suite::from_query_value("zstd"), Suite::Zstd);
        assert_eq!(Suite::from_query_value("zstd-dict"), Suite::ZstdDict);
        assert_eq!(Suite::from_query_value(" ZSTD-DICT "), Suite::ZstdDict);
        assert_eq!(Suite::from_query_value("Plain"), Suite::Plain);
        // 其余任何值(含空串、打错字、长 token)一律默认字典套
        assert_eq!(Suite::from_query_value(""), Suite::ZstdDict);
        assert_eq!(Suite::from_query_value("chat"), Suite::ZstdDict);
        assert_eq!(
            Suite::from_query_value("teamviewrelay.zstd.v1"),
            Suite::ZstdDict
        );
        // wt_protocol_value 保持裸 token 形状(带引号回显会使 Chrome 中止
        // 会话建立,见 42c47f9;回执不被 Chromium 暴露,仅前向兼容保留)
        assert_eq!(
            Suite::ZstdDict.wt_protocol_value(),
            "teamviewrelay.zstd-dict.v1"
        );
    }
}
