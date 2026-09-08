//! 稳定信道抽象(桥接模式,0.9.0-alpha.4 时代确立):对上(应用层
//! `serve_web_map_session`)只交付"可靠有序帧 + 门级能力",三门各自以
//! Adapter 产出同一形态——WS 门(无多流/datagram)与 WT/QUIC 门(全能力)
//! 的差异被封装在各自桥内,上层零感知。
//!
//! UDP 相对 TCP 的优势在抽象层显式为两项能力:
//! - `datagram`:movement 位置批走不可靠 datagram(不重传、不队头阻塞);
//! - `door_control`/`bulk`:额外的独立流(门控/大内容),与应用流互不混用。
//! WebSocket 不具备这些能力,由 WS 适配器报告 `false` 并走等价降级路径。

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::bulk;
use crate::proto::teamviewer::v1::PlayerReportBundle;
use crate::relay::MovementBatch;

/// 门会话能力矩阵。`datagram=false` 时应用层以可靠流低频位置兜底;
/// `bulk=false` 时 bulk-push 触发对该会话不可见。`door_control`/`wire_metrics`
/// 由后续 WS 收编 commit 消费(桥内已按能力装配)。
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
pub(crate) struct DoorCapabilities {
    pub datagram: bool,
    pub door_control: bool,
    pub bulk: bool,
    /// Layer::Wire 计量(WS 门独有,基于 TCP 字节计数)
    pub wire_metrics: bool,
}

/// 三门统一的会话交接形态。入站一帧 = 一个裸 `WireEnvelope`(恒 plain,
/// 分帧/解压已由门桥消化);出站为 plain envelope,压缩与门原生封装
/// (WS 帧 / varint 分帧)在门桥出向完成。
pub(crate) struct DoorSession {
    pub incoming: crate::web::WebMapFrameStream,
    pub outgoing: crate::web_transport::MpscSink,
    /// 转交 relay 注册;门桥侧是消费者(movement datagram 发送)。
    pub movement_tx: watch::Sender<Option<Arc<MovementBatch>>>,
    /// bulk 触发通道(容量 1);`None` = 该门不支持(WS)。
    pub bulk: Option<mpsc::Sender<bulk::BulkRequest>>,
    /// 上行 datagram 位置通道(alpha.5):桥内解析出的 `PlayerReportBundle`
    /// (仅 players_patch 有值);`None` = 该门无 datagram 能力(WS)。
    pub incoming_datagrams: Option<mpsc::Receiver<PlayerReportBundle>>,
    pub capabilities: DoorCapabilities,
}
