//! bulk 传输通道(proto 0.9.0-alpha.4):战区地图等大内容走服务端专用
//! 单向流,与应用下行流、door-control 流互不混用。
//!
//! 低优先级是**写方约定**而非传输原语(quinn/wtransport 均无流优先级
//! API):发送端小块写入(`CHUNK`),可靠应用流自然交错;不按已知链路
//! 预算调任何参数,拥塞控制自行探测路径。
//!
//! 线格式(合同真相源为 TeamViewRelay-Protocol README"bulk 传输通道"):
//! - 通告先于流:`DoorControlFrame{bulk_transfer_start:{transfer_id,
//!   total_len,content_type}}` 经 door-control 下行流下发;
//! - bulk 流自标识:载荷以 `[varint len][transfer_id UTF-8]` 前缀开头,
//!   其后是 `total_len` 字节原始内容。QUIC 不保证跨流顺序,接收端按
//!   `transfer_id` 匹配通告,先于通告到达的字节进有界缓冲。

use std::collections::HashMap;
use std::hash::Hasher;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{Mutex, mpsc};

/// 单次写出让上限:小块写入让应用流自然交错,即"低优先级"的实现。
pub(crate) const CHUNK: usize = 64 * 1024;
/// 接收端"先于通告"缓冲上限建议值(README),发送端按此防御异常重放。
#[cfg_attr(not(feature = "memory-debug"), allow(dead_code))]
pub(crate) const PRE_ANNOUNCE_BUFFER_CAP: usize = 4 * 1024 * 1024;
/// debug 触发的内容防御上限(压测 10 MiB 量级,再翻倍留裕度)。
#[cfg_attr(not(feature = "memory-debug"), allow(dead_code))]
pub(crate) const MAX_BULK_BYTES: usize = 32 * 1024 * 1024;
/// bulk 期间的门控心跳 datagram 周期(供突发压测统计 datagram 送达率)。
pub(crate) const HEARTBEAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);

/// 一次 bulk 传输请求。`content_type` 由触发方自定;突发压测把确定性
/// 图案参数(如 `x-teamviewrelay-burst;seed=42`)编码在这里,接收端据此
/// 重生成期望字节做完整性核对。
#[derive(Clone, Debug)]
pub(crate) struct BulkRequest {
    pub content: Bytes,
    pub content_type: String,
}

/// 活跃门会话的 bulk 触发通道表。会话建立即注册,发送失败(会话已死)
/// 即逐出——debug 通道不做更精细的生命周期管理。
#[derive(Default)]
pub struct Hub {
    sessions: Mutex<HashMap<String, mpsc::Sender<BulkRequest>>>,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::new(Hub::default())
    }

    /// 注册一个门会话,返回列表用展示 ID。
    pub(crate) async fn register(&self, sender: mpsc::Sender<BulkRequest>) -> String {
        let mut sessions = self.sessions.lock().await;
        let id = format!("door-{}", sessions.len() + 1);
        sessions.insert(id.clone(), sender);
        id
    }

    /// 向全部(或指定 ID 的)会话投递 bulk 请求;通道满/关闭视为该会话
    /// 拒绝(已在途或已死),逐出死会话。返回 (accepted, rejected)。
    #[cfg_attr(not(feature = "memory-debug"), allow(dead_code))]
    pub(crate) async fn push(
        &self,
        target: Option<&str>,
        request: BulkRequest,
    ) -> (Vec<String>, Vec<String>) {
        let mut sessions = self.sessions.lock().await;
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        let ids: Vec<String> = sessions
            .keys()
            .filter(|id| target.is_none_or(|want| id.as_str() == want))
            .cloned()
            .collect();
        for id in ids {
            let Some(sender) = sessions.get(&id) else { continue };
            match sender.try_send(request.clone()) {
                Ok(()) => accepted.push(id),
                Err(_) => {
                    sessions.remove(&id);
                    rejected.push(id);
                }
            }
        }
        (accepted, rejected)
    }
}

/// 连接内不透明的传输 ID(内容哈希前缀,与字典 ID 同风格但前缀不同)。
pub(crate) fn transfer_id(content: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&content, &mut hasher);
    format!("b{:016x}", hasher.finish())
}

/// bulk 流自标识前缀:`[varint len][transfer_id UTF-8]`。
pub(crate) fn stream_prefix(transfer_id: &str) -> Vec<u8> {
    let id = transfer_id.as_bytes();
    let mut header = [0u8; 4];
    let header_len = crate::frame::encode_varint(id.len(), &mut header);
    let mut out = Vec::with_capacity(header_len + id.len());
    out.extend_from_slice(&header[..header_len]);
    out.extend_from_slice(id);
    out
}

/// bulk 期间的门控心跳 datagram:固定魔数 + BE 序号,接收端按送达序号
/// 统计送达率。仅在 debug bulk 路径启用,普通会话不可见。
pub(crate) fn heartbeat(seq: u32) -> Bytes {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(b"tvbulk-hb");
    out.extend_from_slice(&seq.to_be_bytes());
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_round_trips_via_varint_rule() {
        let id = transfer_id(b"payload");
        assert!(id.starts_with('b') && id.len() == 17);
        let prefix = stream_prefix(&id);
        // varint 头 1 字节(短 ID)+ ID 本体
        assert_eq!(prefix.len(), 1 + id.len());
        assert_eq!(prefix[0] as usize, id.len());
        assert_eq!(&prefix[1..], id.as_bytes());
    }

    #[test]
    fn transfer_id_is_content_sensitive() {
        assert_ne!(transfer_id(b"one"), transfer_id(b"two"));
    }

    #[test]
    fn heartbeat_carries_magic_and_sequence() {
        let wire = heartbeat(7);
        assert_eq!(&wire[..9], b"tvbulk-hb");
        assert_eq!(&wire[9..], &7u32.to_be_bytes());
    }

    #[tokio::test]
    async fn hub_registers_pushes_and_prunes_dead() {
        let hub = Hub::new();
        let (tx, mut rx) = mpsc::channel(1);
        let id = hub.register(tx).await;
        let (accepted, rejected) = hub
            .push(
                None,
                BulkRequest {
                    content: Bytes::from_static(b"x"),
                    content_type: "test".into(),
                },
            )
            .await;
        assert_eq!(accepted, vec![id.clone()]);
        assert!(rejected.is_empty());
        // 通道容量 1:第二条被拒且会话被逐出
        let (accepted, rejected) = hub
            .push(
                None,
                BulkRequest {
                    content: Bytes::from_static(b"y"),
                    content_type: "test".into(),
                },
            )
            .await;
        assert!(accepted.is_empty() && rejected.len() == 1);
        // 首条请求仍可消费
        assert!(rx.recv().await.is_some());
        // 定向投递未知 ID:不误伤
        let (accepted, rejected) = hub
            .push(
                Some("door-999"),
                BulkRequest {
                    content: Bytes::from_static(b"z"),
                    content_type: "test".into(),
                },
            )
            .await;
        assert!(accepted.is_empty() && rejected.is_empty());
    }
}
