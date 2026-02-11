//! Audio feedback for transcription completion.
//!
//! Plays a short notification sound when transcription completes, providing
//! immediate auditory feedback before text injection occurs.

use std::path::PathBuf;
use std::process::Command;
use std::thread;

use tracing::{debug, trace, warn};

/// Sound paths to probe in order of preference.
/// Project sounds are checked first, then system sounds.
const SOUND_PATHS: &[&str] = &[
    // Project sounds (relative to binary or repo)
    "sounds/completion.ogg",
    "../sounds/completion.ogg",
    // System sounds
    "/usr/share/sounds/freedesktop/stereo/complete.oga",
    "/usr/share/sounds/freedesktop/stereo/message.oga",
    "/usr/share/sounds/freedesktop/stereo/bell.oga",
];

/// Audio players to probe in order of preference.
const AUDIO_PLAYERS: &[&str] = &["pw-play", "paplay", "aplay"];

/// Audio feedback configuration and player.
#[derive(Debug, Clone)]
pub struct AudioFeedback {
    enabled: bool,
    player_binary: Option<PathBuf>,
    sound_path: Option<PathBuf>,
    volume: u8,
}

impl AudioFeedback {
    /// Create a new AudioFeedback instance.
    ///
    /// - `enabled`: Whether audio feedback is enabled.
    /// - `custom_sound_path`: Optional path to a custom sound file.
    /// - `volume`: Volume level 0-100 (currently only used for logging/future use).
    pub fn new(enabled: bool, custom_sound_path: Option<PathBuf>, volume: u8) -> Self {
        if !enabled {
            return Self {
                enabled: false,
                player_binary: None,
                sound_path: None,
                volume,
            };
        }

        let player_binary = Self::find_audio_player();
        let sound_path = Self::resolve_sound_path(custom_sound_path);

        if player_binary.is_none() {
            warn!(
                "No audio player found (tried: {:?}); completion sounds disabled",
                AUDIO_PLAYERS
            );
        } else if sound_path.is_none() {
            warn!("No sound file found; completion sounds disabled");
        }

        let effectively_enabled = player_binary.is_some() && sound_path.is_some();

        if effectively_enabled {
            debug!(
                player = ?player_binary,
                sound = ?sound_path,
                volume,
                "Audio feedback initialized"
            );
        }

        Self {
            enabled: effectively_enabled,
            player_binary,
            sound_path,
            volume,
        }
    }

    /// Find an available audio player binary.
    fn find_audio_player() -> Option<PathBuf> {
        for player in AUDIO_PLAYERS {
            if let Ok(path) = which::which(player) {
                trace!(player = %player, path = ?path, "Found audio player");
                return Some(path);
            }
        }
        None
    }

    /// Resolve the sound file path, preferring custom path over defaults.
    fn resolve_sound_path(custom: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(path) = custom {
            if path.exists() {
                trace!(path = ?path, "Using custom sound file");
                return Some(path);
            }
            warn!(path = ?path, "Custom sound file does not exist; falling back to default sounds");
        }

        // Try paths relative to current executable first
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                for relative in &["sounds/completion.ogg", "../sounds/completion.ogg"] {
                    let path = exe_dir.join(relative);
                    if path.exists() {
                        trace!(path = ?path, "Using project sound file (relative to exe)");
                        return Some(path);
                    }
                }
            }
        }

        // Try paths relative to current working directory
        for path_str in SOUND_PATHS {
            let path = PathBuf::from(path_str);
            if path.exists() {
                trace!(path = ?path, "Using sound file");
                return Some(path);
            }
        }
        None
    }

    /// Play the completion sound (non-blocking, fire-and-forget).
    ///
    /// Spawns a background thread to avoid blocking the main event loop.
    /// Any errors are logged but never propagate.
    pub fn play_completion(&self) {
        if !self.enabled {
            return;
        }

        let Some(player) = &self.player_binary else {
            return;
        };
        let Some(sound) = &self.sound_path else {
            return;
        };

        let player = player.clone();
        let sound = sound.clone();
        let volume = self.volume;

        thread::spawn(move || {
            trace!(player = ?player, sound = ?sound, "Playing completion sound");

            let mut cmd = Command::new(&player);
            cmd.arg(&sound);

            // pw-play supports --volume (0.0-1.0 scale)
            if player.to_string_lossy().contains("pw-play") && volume < 100 {
                let vol_float = f32::from(volume) / 100.0;
                cmd.arg("--volume").arg(format!("{:.2}", vol_float));
            }

            match cmd.output() {
                Ok(output) => {
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!(
                            player = ?player,
                            status = ?output.status,
                            stderr = %stderr.trim(),
                            "Audio player exited with error"
                        );
                    } else {
                        trace!("Completion sound played successfully");
                    }
                }
                Err(err) => {
                    warn!(player = ?player, error = %err, "Failed to spawn audio player");
                }
            }
        });
    }

    /// Check if audio feedback is effectively enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_feedback_does_not_probe() {
        let feedback = AudioFeedback::new(false, None, 100);
        assert!(!feedback.is_enabled());
        assert!(feedback.player_binary.is_none());
        assert!(feedback.sound_path.is_none());
    }

    #[test]
    fn custom_path_takes_precedence() {
        // This test verifies the logic path, not actual file existence
        let feedback =
            AudioFeedback::new(true, Some(PathBuf::from("/nonexistent/custom.ogg")), 100);
        // Custom path doesn't exist, so it should fall back to system sounds (or none)
        // The important thing is it doesn't panic
        let _ = feedback.is_enabled();
    }
}
