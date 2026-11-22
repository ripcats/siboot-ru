use std::path::{Path, PathBuf};

use crate::bridge::{Bridge, BridgeError};

pub const PAIRING_SECONDS: u32 = 16 * 60;

const SAMPLE_RATE: u32 = 8_000;

pub fn silent_wav(seconds: u32) -> Vec<u8> {
    let data_len = SAMPLE_RATE * seconds;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&8u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0x80);
    wav
}

pub struct KeepAlive {
    bridge: Bridge,
    file: PathBuf,
    session: Option<String>,
}

impl KeepAlive {
    pub async fn start(bridge: &Bridge, home: &Path, seconds: u32) -> Result<Self, String> {
        let file = silence_file(home, seconds);
        let wanted = 44 + u64::from(SAMPLE_RATE * seconds);
        let present = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        if present != wanted {
            std::fs::write(&file, silent_wav(seconds))
                .map_err(|e| format!("не удалось записать {}: {e}", file.display()))?;
        }
        let session = bridge.play(&file).await.map_err(|e| match e {
            BridgeError::Missing(_) => "В Open Minis недоступна команда apple-player".to_string(),
            other => other.to_string(),
        })?;
        Ok(Self {
            bridge: bridge.clone(),
            file,
            session: Some(session),
        })
    }

    pub async fn revive(&mut self) -> Result<bool, String> {
        let Some(session) = self.session.clone() else {
            return Ok(false);
        };
        match self.bridge.player_status(&session).await {
            Ok(status) if status == "playing" => Ok(false),
            Ok(_) => self
                .bridge
                .resume_player(&session)
                .await
                .map(|()| true)
                .map_err(|e| e.to_string()),
            Err(BridgeError::Refused { .. }) => {
                let again = self
                    .bridge
                    .play(&self.file)
                    .await
                    .map_err(|e| e.to_string())?;
                self.session = Some(again);
                Ok(true)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub async fn stop(mut self) {
        if let Some(session) = self.session.take() {
            if let Err(e) = self.bridge.stop_player(&session).await {
                tracing::warn!("не удалось остановить фоновое аудио ({session}): {e}");
            }
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            self.bridge.call_now("apple-player", &["stop", &session]);
        }
    }
}

fn silence_file(home: &Path, seconds: u32) -> PathBuf {
    home.join(format!("silence-{seconds}s.wav"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_silence_is_a_well_formed_wav() {
        let wav = silent_wav(2);
        assert_eq!(wav.len(), 44 + 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize,
            wav.len() - 8
        );
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1, "PCM");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 8_000);
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 8, "bits");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 16_000);
        assert!(
            wav[44..].iter().all(|&b| b == 0x80),
            "silence is the midpoint for 8-bit PCM"
        );
    }

    #[test]
    fn pairing_fits_inside_the_silence() {
        const { assert!(PAIRING_SECONDS >= 15 * 60) };
    }
}
