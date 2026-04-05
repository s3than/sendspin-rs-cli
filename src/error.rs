use thiserror::Error;

#[derive(Debug, Error)]
pub enum SendspinError {
    #[error("invalid volume {0}: must be 0-100")]
    InvalidVolume(u8),

    #[error("mDNS discovery failed: {0}")]
    Discovery(String),

    #[error("connection failed: {0}")]
    Connection(String),

    #[error("audio error: {0}")]
    Audio(String),

    #[error("no output device available")]
    NoOutputDevice,

    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Cpal(#[from] cpal::BuildStreamError),

    #[error(transparent)]
    CpalPlay(#[from] cpal::PlayStreamError),

    #[error(transparent)]
    CpalDevice(#[from] cpal::DefaultStreamConfigError),

    #[error(transparent)]
    Mdns(#[from] mdns_sd::Error),
}
