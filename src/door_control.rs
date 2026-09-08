//! 门级 door-control 信道与 datagram 字典编码(alpha.6)。
//!
//! door-control 是门适配层私有的控制信道(各一条单向流,分帧与流通道同款),
//! 绝不承载应用层 `WireEnvelope`(proto 0.9.0-alpha.2"传输门约定"),应用层与
//! 会话处理器对它无感知。当前唯一载荷是 datagram 字典协商:`+zstd-dict` 套下
//! 服务端用近期 movement 批训练 4KB 字典,经 door-control 下行流下发
//! `dict_offer`,客户端完整安装后回 `dict_ready`,服务端收到 ready 才把该字典
//! 切为压缩当前字典(激活)——因果屏障保证字典字节必然先于用它压缩的任何
//! datagram 过线。激活前 datagram 按独立(无字典)zstd 单帧压缩;zstd 帧自
//! 包含,解码端按确定状态机处理("已安装字典则用字典解,否则独立解"),唯一
//! 竞态(客户端 door-control 安装任务滞后于 datagram 接收)表现为响亮的
//! zstd 错误,按普通丢包丢帧自愈——movement 下一 tick 自愈 + 低频保底全量。
//!
//! 字典生命周期(防泄漏):每连接只保留 current + previous 两个 ID(≤8KB),
//! 新 ID 激活即逐出更旧,连接关闭全部释放;字典内容按连接独立,不跨连接共享
//! 或缓存。重训设最小间隔与样本阈值,防重训风暴。

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tracing::debug;

use crate::compress::{STREAM_COMPRESSION_LEVEL, Suite};
// 门控帧自 proto 0.9.0-alpha.4 起归属传输层包 teamviewer.door.v1
// (字段号与应用层迁移前逐一不变,线上字节零变化)。
use crate::proto::teamviewer::door::v1::{
    BulkTransferStart, DatagramDictOffer, DoorControlFrame,
    door_control_frame::Payload as DoorControlPayload,
};
use crate::relay::MovementBatch;

/// 训练字典大小(实测甜点,见协议 README"door-control 流")。
pub(crate) const DICT_SIZE: usize = 4096;
/// 字典内容防御上限:超限视为协议违规,拒发也拒装。
pub(crate) const MAX_DICT_CONTENT: usize = 16 * 1024;
/// 生产训练阈值:攒够样本才训练,保证字典对后续 movement 批有代表性。
const DEFAULT_TRAIN_CHUNKS: usize = 64;
/// 生产样本字节下限:zstd 训练器要求样本总量不低于字典大小才肯工作
/// (不足时报 "Src size is incorrect"),这里抬高到 4 倍字典大小,稀疏
/// 场景(少量小批)宁可延迟训练也不空转浪费 CPU。
const DEFAULT_MIN_SAMPLE_BYTES: usize = DICT_SIZE * 4;
/// 生产重训最小间隔:防重训风暴。
const DEFAULT_MIN_RETRAIN: Duration = Duration::from_secs(60);

/// 训练触发策略(测试可注入更小阈值/零间隔)。
#[derive(Clone, Copy)]
pub(crate) struct TrainingPolicy {
    chunks_threshold: usize,
    min_sample_bytes: usize,
    min_interval: Duration,
}

impl TrainingPolicy {
    pub(crate) fn production() -> TrainingPolicy {
        TrainingPolicy {
            chunks_threshold: DEFAULT_TRAIN_CHUNKS,
            min_sample_bytes: DEFAULT_MIN_SAMPLE_BYTES,
            min_interval: DEFAULT_MIN_RETRAIN,
        }
    }
}

/// 训练产出:待经 door-control 下行流下发的字典。
pub(crate) struct TrainedOffer {
    pub(crate) id: String,
    pub(crate) content: Bytes,
}

struct ActiveDict {
    id: String,
    // 服务端压缩只用 current;previous 仅为对齐"current+previous"生命周期的
    // 显式占位(与客户端解压宽限对称),持有了 CDict 即真实持有 ≤4KB 字典
    _cdict: zstd_safe::CDict<'static>,
}

/// 每连接一个的 datagram 字典编码器,由门的 movement 发送任务独占使用。
/// `plain` 套下完全惰性(不积累样本、不训练、原样透传)。
pub(crate) struct DatagramDictEncoder {
    suite: Suite,
    cctx: zstd_safe::CCtx<'static>,
    current: Option<ActiveDict>,
    previous: Option<ActiveDict>,
    pending: Option<ActiveDict>,
    samples: Vec<Vec<u8>>,
    policy: TrainingPolicy,
    last_training: Option<Instant>,
}

impl DatagramDictEncoder {
    /// 生产构造:默认阈值与重训间隔。
    pub(crate) fn for_suite(suite: Suite) -> DatagramDictEncoder {
        Self::with_policy(suite, TrainingPolicy::production())
    }

    pub(crate) fn with_policy(suite: Suite, policy: TrainingPolicy) -> DatagramDictEncoder {
        let mut cctx = zstd_safe::CCtx::create();
        // 单帧独立压缩级别与流压缩同款;CDict 自带级别,此参数只作用于
        // 激活前的无字典路径
        let _ = cctx.set_parameter(zstd_safe::CParameter::CompressionLevel(
            STREAM_COMPRESSION_LEVEL,
        ));
        DatagramDictEncoder {
            suite,
            cctx,
            current: None,
            previous: None,
            pending: None,
            samples: Vec::new(),
            policy,
            last_training: None,
        }
    }

    /// datagram 压缩是否启用(`plain` 套恒否)。
    pub(crate) fn compression_enabled(&self) -> bool {
        self.suite.datagram_zstd()
    }

    /// 字典协商是否参与(`+zstd-dict` 套独有;`+zstd` 套永远独立压缩)。
    pub(crate) fn dict_enabled(&self) -> bool {
        matches!(self.suite, Suite::ZstdDict)
    }

    #[cfg(test)]
    pub(crate) fn current_dict_id(&self) -> Option<&str> {
        self.current.as_ref().map(|dict| dict.id.as_str())
    }

    #[cfg(test)]
    pub(crate) fn previous_dict_id(&self) -> Option<&str> {
        self.previous.as_ref().map(|dict| dict.id.as_str())
    }

    /// 累积 movement 样本;达到条数阈值、样本字节下限且距上次训练超过
    /// 最小间隔时训练新字典,返回待下发的 offer。`now` 由调用方传入以便
    /// 测试控制时钟。
    pub(crate) fn observe(&mut self, batch: &MovementBatch, now: Instant) -> Option<TrainedOffer> {
        if !self.dict_enabled() {
            return None;
        }
        for chunk in batch.chunks.iter() {
            self.samples.push(chunk.to_vec());
        }
        // 样本缓冲上限:pending 未决期间(ready 迟迟不来)样本会持续流入,
        // 超过阈值 4 倍即丢最旧一半,封住内存增长
        if self.samples.len() > self.policy.chunks_threshold * 4 {
            let keep_from = self.samples.len() / 2;
            self.samples.drain(..keep_from);
        }
        let sample_bytes: usize = self.samples.iter().map(|sample| sample.len()).sum();
        if self.pending.is_some()
            || self.samples.len() < self.policy.chunks_threshold
            || sample_bytes < self.policy.min_sample_bytes
            || self
                .last_training
                .is_some_and(|at| now.duration_since(at) < self.policy.min_interval)
        {
            return None;
        }
        // 训练失败(样本总量仍太小、分布退化等)按一次尝试计:过最小间隔
        // 再重试,并丢最旧一半样本防失败后原地打转
        let content = match zstd::dict::from_samples(&self.samples, DICT_SIZE) {
            Ok(content) => content,
            Err(error) => {
                debug!(%error, "datagram dictionary training failed");
                let keep_from = self.samples.len() / 2;
                self.samples.drain(..keep_from);
                self.last_training = Some(now);
                return None;
            }
        };
        self.samples.clear();
        self.last_training = Some(now);
        let id = dict_id(&content);
        let cdict = zstd_safe::CDict::create(&content, STREAM_COMPRESSION_LEVEL);
        self.pending = Some(ActiveDict {
            id: id.clone(),
            _cdict: cdict,
        });
        Some(TrainedOffer {
            id,
            content: Bytes::from(content),
        })
    }

    /// 收到客户端 `dict_ready(ID)`:pending 字典提升为压缩当前字典,旧
    /// current 降为 previous(乱序在途帧的宽限),更旧字典即被逐出释放。
    /// 未知或重复 ID 直接忽略(不影响两端状态机)。
    pub(crate) fn activate(&mut self, id: &str) {
        if self.pending.as_ref().is_some_and(|dict| dict.id == id) {
            let dict = self.pending.take().expect("pending checked above");
            self.previous = self.current.take();
            self.current = Some(dict);
        }
    }

    /// 编码一个 datagram 载荷:`plain` 原样;`+zstd`/`+zstd-dict` 按当前状态
    /// 单帧压缩(自包含,无跨 datagram 上下文,丢一块不影响后续)。zstd
    /// 错误返回 None,调用方按"装不下"跳过——下个 dirty tick 全量重发自愈。
    pub(crate) fn encode(&mut self, chunk: &[u8]) -> Option<Bytes> {
        if !self.compression_enabled() {
            return Some(Bytes::copy_from_slice(chunk));
        }
        let mut out = vec![0u8; zstd_safe::compress_bound(chunk.len())];
        let written = match self.current.as_ref() {
            Some(dict) => self
                .cctx
                .compress_using_cdict(&mut out, chunk, &dict._cdict),
            None => self.cctx.compress2(&mut out, chunk),
        };
        let written = written.ok()?;
        out.truncate(written);
        Some(Bytes::from(out))
    }
}

/// 字典 ID:内容哈希前缀,连接内不透明标识(客户端只原样回执)。
fn dict_id(content: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    format!("d{:016x}", hasher.finish())
}

/// 序列化一个 `dict_offer` 门控制帧;内容超防御上限时拒绝。
pub(crate) fn dict_offer_frame(id: &str, content: &[u8]) -> Option<Bytes> {
    if content.len() > MAX_DICT_CONTENT {
        return None;
    }
    Some(encode_frame(&DoorControlFrame {
        payload: Some(DoorControlPayload::DictOffer(DatagramDictOffer {
            dictionary_id: id.to_owned(),
            content: content.to_vec(),
        })),
    }))
}

/// 通告一次 bulk 传输(经 door-control 下行流下发)。
pub(crate) fn bulk_start_frame(transfer_id: &str, total_len: u64, content_type: &str) -> Bytes {
    encode_frame(&DoorControlFrame {
        payload: Some(DoorControlPayload::BulkTransferStart(BulkTransferStart {
            transfer_id: transfer_id.to_owned(),
            total_len,
            content_type: content_type.to_owned(),
        })),
    })
}

/// 门控帧严格解析:非法 protobuf 返回 None(调用方按协议违规断连)。
/// 解析成功但携带未知 oneof 成员属前向兼容,返回 Some。
pub(crate) fn decode_frame(payload: &[u8]) -> Option<DoorControlFrame> {
    use prost::Message as _;
    DoorControlFrame::decode(payload).ok()
}

fn encode_frame(frame: &DoorControlFrame) -> Bytes {
    use prost::Message as _;
    let mut buf = Vec::with_capacity(frame.encoded_len());
    frame
        .encode(&mut buf)
        .expect("prost encode into Vec is infallible");
    Bytes::from(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::teamviewer::door::v1::DatagramDictReady;
    use std::sync::Arc;

    fn batch_of(chunks: &[&[u8]]) -> MovementBatch {
        MovementBatch {
            chunks: chunks
                .iter()
                .map(|chunk| Arc::from(chunk.to_vec().into_boxed_slice()))
                .collect::<Vec<_>>()
                .into(),
        }
    }

    /// 测试时钟:`Instant::now()` 之后的毫秒偏移(只向后走,避免单调时钟
    /// 减法下溢;observe 的 `now` 由调用方传入,相对顺序即时钟语义)。
    fn t(millis: u64) -> Instant {
        Instant::now() + Duration::from_millis(millis)
    }

    fn small_policy() -> TrainingPolicy {
        TrainingPolicy {
            chunks_threshold: 12,
            min_sample_bytes: DICT_SIZE,
            min_interval: Duration::ZERO,
        }
    }

    // zstd 训练器对样本条数有下限(实测 12 条以下直接拒绝"Src size is
    // incorrect"),测试样本一次给足条数并带填充
    fn padded_samples(prefix: &str, count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|index| {
                let mut sample = format!("{prefix}-{index}").into_bytes();
                sample.resize(sample.len() + DICT_SIZE / 4, b'x');
                sample
            })
            .collect()
    }

    fn sample_refs(samples: &[Vec<u8>]) -> Vec<&[u8]> {
        samples.iter().map(|sample| sample.as_slice()).collect()
    }

    #[test]
    fn plain_suite_is_fully_inert() {
        let mut encoder = DatagramDictEncoder::with_policy(Suite::Plain, small_policy());
        assert!(!encoder.compression_enabled() && !encoder.dict_enabled());
        // 不积累样本、不训练、原样透传
        assert!(encoder.observe(&batch_of(&[b"aaaa"]), t(0)).is_none());
        assert_eq!(
            encoder.encode(b"raw-envelope").expect("passthrough"),
            Bytes::from_static(b"raw-envelope")
        );
        assert!(encoder.current_dict_id().is_none());
    }

    #[test]
    fn offer_ready_activation_gates_dictionary_compression() {
        let mut encoder = DatagramDictEncoder::with_policy(Suite::ZstdDict, small_policy());
        assert!(encoder.dict_enabled());
        // 阈值未到:不训练,datagram 走独立压缩(可无字典解出)
        let first = encoder
            .encode(b"before-training")
            .expect("independent encode");
        assert!(zstd::stream::decode_all(&first[..]).is_ok());

        let warmup = padded_samples("sample-warm", 6);
        let training = padded_samples("sample-train", 8);
        assert!(
            encoder
                .observe(&batch_of(&sample_refs(&warmup)), t(0))
                .is_none()
        );
        let offer = encoder
            .observe(&batch_of(&sample_refs(&training)), t(0))
            .expect("trained at threshold");
        assert!(offer.content.len() <= MAX_DICT_CONTENT);
        // 训练后、ready 前:仍是独立压缩
        let pre = encoder.encode(b"not-yet-active").expect("pre activation");
        assert!(zstd::stream::decode_all(&pre[..]).is_ok());

        // 错误 ID 的 ready 不激活
        encoder.activate("d-wrong");
        assert!(encoder.current_dict_id().is_none());
        // 正确 ID 激活后:无字典解不开,带字典才能解出
        encoder.activate(&offer.id);
        assert_eq!(encoder.current_dict_id(), Some(offer.id.as_str()));
        let post = encoder.encode(b"dict-compressed-now").expect("dict encode");
        assert!(
            zstd::stream::decode_all(&post[..]).is_err(),
            "无字典必须解不开"
        );
        let mut dctx = zstd_safe::DCtx::create();
        let ddict = zstd_safe::DDict::create(&offer.content);
        let mut out = vec![0u8; 128];
        let written = dctx
            .decompress_using_ddict(&mut out, &post, &ddict)
            .expect("dict decompress");
        assert_eq!(&out[..written], b"dict-compressed-now");
    }

    #[test]
    fn lifecycle_keeps_only_current_and_previous() {
        let mut encoder = DatagramDictEncoder::with_policy(Suite::ZstdDict, small_policy());
        let train = |encoder: &mut DatagramDictEncoder, epoch: u64| {
            let mut samples = padded_samples(&format!("sample-{epoch}"), 10);
            samples.extend(padded_samples("shared-tail", 2));
            let offer = encoder
                .observe(&batch_of(&sample_refs(&samples)), t(1_000 * epoch))
                .expect("trained");
            encoder.activate(&offer.id);
            offer
        };
        let first = train(&mut encoder, 1);
        assert!(encoder.previous_dict_id().is_none());
        let second = train(&mut encoder, 2);
        assert_eq!(encoder.current_dict_id(), Some(second.id.as_str()));
        assert_eq!(encoder.previous_dict_id(), Some(first.id.as_str()));
        let third = train(&mut encoder, 3);
        // 第三个激活即逐出更旧:只剩 current(第三) + previous(第二)
        assert_eq!(encoder.current_dict_id(), Some(third.id.as_str()));
        assert_eq!(encoder.previous_dict_id(), Some(second.id.as_str()));
    }

    #[test]
    fn min_retrain_interval_and_pending_defer_training() {
        let mut encoder = DatagramDictEncoder::with_policy(
            Suite::ZstdDict,
            TrainingPolicy {
                chunks_threshold: 12,
                min_sample_bytes: DICT_SIZE,
                min_interval: Duration::from_secs(60),
            },
        );
        let base = t(0);
        let batches: Vec<Vec<Vec<u8>>> = ["first", "second", "third", "fourth"]
            .iter()
            .map(|prefix| padded_samples(prefix, 12))
            .collect();
        let refs: Vec<Vec<&[u8]>> = batches.iter().map(|batch| sample_refs(batch)).collect();
        let first = encoder
            .observe(&batch_of(&refs[0]), base)
            .expect("first train allowed");
        // 间隔未到不重训
        assert!(
            encoder
                .observe(&batch_of(&refs[1]), base + Duration::from_secs(59))
                .is_none()
        );
        // 间隔到了但 pending(未 ready)仍未清:也不重训
        assert!(
            encoder
                .observe(&batch_of(&refs[2]), base + Duration::from_secs(61))
                .is_none()
        );
        // ready 之后,间隔已过:重训出新 ID
        encoder.activate(&first.id);
        let second = encoder
            .observe(&batch_of(&refs[3]), base + Duration::from_secs(61))
            .expect("retrain after ready + interval");
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn door_control_frames_round_trip() {
        let content = vec![7u8; DICT_SIZE];
        let id = dict_id(&content);
        let wire = dict_offer_frame(&id, &content).expect("offer frame");
        let decoded = decode_frame(&wire).expect("decode");
        match decoded.payload.expect("payload") {
            DoorControlPayload::DictOffer(DatagramDictOffer {
                dictionary_id,
                content: offered,
            }) => {
                assert_eq!(dictionary_id, id);
                assert_eq!(offered, content);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        let ready = encode_frame(&DoorControlFrame {
            payload: Some(DoorControlPayload::DictReady(DatagramDictReady {
                dictionary_id: id.clone(),
            })),
        });
        let ready_id = match decode_frame(&ready).and_then(|frame| frame.payload) {
            Some(DoorControlPayload::DictReady(ready)) => Some(ready.dictionary_id),
            _ => None,
        };
        assert_eq!(ready_id, Some(id.clone()));
        // 未知 oneof 成员之外:非字典载荷不误读为 ready;非法 protobuf = None(违规)
        assert!(matches!(
            decode_frame(&wire).and_then(|frame| frame.payload),
            Some(DoorControlPayload::DictOffer(_))
        ));
        assert!(decode_frame(b"garbage").is_none());
    }

    #[test]
    fn oversized_dict_content_is_rejected() {
        let id = "d-test";
        assert!(dict_offer_frame(id, &vec![0u8; MAX_DICT_CONTENT + 1]).is_none());
        assert!(dict_offer_frame(id, &vec![0u8; MAX_DICT_CONTENT]).is_some());
    }

    #[test]
    fn bulk_start_frame_round_trip_and_field_numbers_unchanged() {
        let frame = decode_frame(
            &bulk_start_frame("babc", 10 * 1024 * 1024, "x-teamviewrelay-burst;seed=42"),
        )
        .expect("bulk frame decode");
        match frame.payload.expect("bulk payload") {
            DoorControlPayload::BulkTransferStart(start) => {
                assert_eq!(start.transfer_id, "babc");
                assert_eq!(start.total_len, 10 * 1024 * 1024);
                assert_eq!(start.content_type, "x-teamviewrelay-burst;seed=42");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    /// 分层兜底实证:跨层混用绝不会被误读为对侧合法载荷——要么解析
    /// 当场报错(prost 对 oneof 成员 wire type 不匹配直接报错),要么
    /// 解出"无载荷"壳子由对侧语义规则(10s 首帧握手)显式拒绝。
    /// 防线是"解析失败或 payload 缺失即违规",不是单点解析错误。
    #[test]
    fn door_and_app_frames_are_mutually_unparseable() {
        use crate::proto::teamviewer::v1::WireEnvelope;
        use prost::Message as _;

        // dict_offer(字段 1, length-delimited)撞上 WireEnvelope.channel
        // (字段 1, varint)的 wire type:app 侧解析直接报错
        let content = vec![7u8; 64];
        let id = dict_id(&content);
        let dict_frame = dict_offer_frame(&id, &content).expect("offer frame");
        assert!(WireEnvelope::decode(dict_frame.as_ref()).is_err());

        // bulk_start(字段 3)在 app 侧落入 unknown fields:解出的是
        // "channel 未指定 + 无 payload"壳子,过不了 10s 首帧握手即断连
        let bulk_frame = bulk_start_frame("babc", 1, "test");
        let as_app = WireEnvelope::decode(bulk_frame.as_ref()).expect("unknown-field shell");
        assert_eq!(
            as_app.channel,
            crate::proto::teamviewer::v1::WireChannel::Unspecified as i32
        );
        assert!(as_app.payload.is_none());

        // 反向:envelope(字段 1 varint)撞上 DoorControlFrame oneof 成员
        // dict_offer(字段 1, length-delimited)的 wire type:prost 对
        // oneof 成员的 wire type 不匹配直接报错——door 侧解析当场失败
        let app = WireEnvelope {
            channel: crate::proto::teamviewer::v1::WireChannel::WebMap as i32,
            payload: None,
        };
        let mut buf = Vec::with_capacity(app.encoded_len());
        app.encode(&mut buf).expect("envelope encode");
        assert!(decode_frame(&buf).is_none());
    }
}
