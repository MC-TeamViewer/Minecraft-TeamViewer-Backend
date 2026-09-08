#[allow(clippy::large_enum_variant)]
pub mod teamviewer {
    // 应用层协议(0.9.0-alpha.4 起 door 控制帧迁出至传输层 door 包)
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/teamviewer.v1.rs"));
    }
    // 传输层门控协议(TeamViewRelay-Protocol proto/teamviewer/door/v1)
    pub mod door {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/teamviewer.door.v1.rs"));
        }
    }
}
