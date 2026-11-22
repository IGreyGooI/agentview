use url::Url;

use super::QwenRealtimeConfigError;

/// A publicly retrievable 16 kHz PCM or WAV sample for Qwen speaker enhancement.
///
/// Qwen fetches this URL asynchronously after the first Smart Turn session
/// update. The caller owns recording format, URL availability and any signed
/// URL lifetime. Registration outcomes are exposed by the raw server event
/// stream as `voiceprint_audio_list.in_progress`, `completed` and `failed`.
#[derive(Clone)]
pub struct QwenRealtimeVoiceprintAudioUrl(Url);

impl QwenRealtimeVoiceprintAudioUrl {
    pub fn new(url: Url) -> Result<Self, QwenRealtimeConfigError> {
        // The application owns this fetch URL. An unsupported scheme or an
        // ambiguous credential/fragment component cannot represent the intended
        // sample fetch. Refuse only this optional configuration before mounting,
        // allowing the caller to continue its ordinary microphone conversation.
        if !matches!(url.scheme(), "http" | "https")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(QwenRealtimeConfigError::InvalidVoiceprintAudioUrl);
        }
        Ok(Self(url))
    }

    pub(super) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_urls_preserve_signed_queries_and_support_http() {
        for address in [
            "https://audio.example/speaker.wav?expires=123&signature=a%2Bb",
            "http://audio.example/speaker.pcm",
        ] {
            let url = QwenRealtimeVoiceprintAudioUrl::new(Url::parse(address).unwrap()).unwrap();
            assert_eq!(url.as_str(), address);
        }
    }

    #[test]
    fn unsupported_fetch_addresses_refuse_only_optional_configuration() {
        for address in [
            "file:///tmp/speaker.wav",
            "data:audio/wav;base64,AA==",
            "https://user:secret@audio.example/speaker.wav",
            "https://audio.example/speaker.wav#part",
        ] {
            assert_eq!(
                QwenRealtimeVoiceprintAudioUrl::new(Url::parse(address).unwrap()).err(),
                Some(QwenRealtimeConfigError::InvalidVoiceprintAudioUrl)
            );
        }
    }
}
