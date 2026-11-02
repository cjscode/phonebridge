use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityPreset {
    #[default]
    Wifi,
    /// Low-data preset for when the phone is on mobile data.
    Cellular,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PresetParams {
    /// `--max-size`: longest edge of the encoded video.
    pub max_size: u32,
    /// `--video-bit-rate`
    pub video_bit_rate: &'static str,
    /// `--max-fps`
    pub max_fps: u32,
    /// `--audio-bit-rate`
    pub audio_bit_rate: &'static str,
    /// `--new-display` geometry for per-app windows: WxH/dpi (portrait).
    pub app_display: &'static str,
}

impl QualityPreset {
    pub fn params(self) -> PresetParams {
        match self {
            QualityPreset::Wifi => PresetParams {
                max_size: 1920,
                video_bit_rate: "8M",
                max_fps: 60,
                audio_bit_rate: "128K",
                app_display: "1080x1920/420",
            },
            QualityPreset::Cellular => PresetParams {
                max_size: 1024,
                video_bit_rate: "1500K",
                max_fps: 30,
                audio_bit_rate: "64K",
                app_display: "720x1280/320",
            },
        }
    }
}
