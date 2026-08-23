#[allow(clippy::large_enum_variant)]
pub mod teamviewer {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/teamviewer.v1.rs"));
    }
}
