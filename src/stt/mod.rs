pub mod fasterwhisper;
pub mod groq;
pub mod whisper;

use crate::config::{Config, SttBackend};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait SttProvider: Send + Sync {
    /// Takes 16kHz mono i16 PCM as a WAV byte blob, returns transcript text.
    async fn transcribe(&self, wav_bytes: Vec<u8>) -> anyhow::Result<Transcript>;

    /// The one retry after `transcribe` came back without words. A backend
    /// whose decoder can collapse decodes differently here; the default asks
    /// again.
    async fn retranscribe(&self, wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
        self.transcribe(wav_bytes).await
    }
}

pub struct Transcript {
    pub text: String,
    pub raw_json: String,
    #[allow(dead_code)]
    pub latency_ms: u128,
    /// The decoder skipped a stretch of audio: part of the recording came back
    /// as punctuation only, so its words are missing from `text`.
    pub skipped_audio: bool,
}

impl Transcript {
    /// Every stretch of the audio came back as words.
    fn complete(&self) -> bool {
        has_words(&self.text) && !self.skipped_audio
    }
}

/// Whether `text` is a transcription at all: it has a letter or a digit, not
/// only punctuation and whitespace such as a lone ",".
pub fn has_words(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// Transcribes speech, retrying once when the result has no words or skipped
/// part of the audio. `None` means even the retry heard no words: the caller
/// says so instead of pasting a stray ",".
pub async fn transcribe_speech(
    stt: &dyn SttProvider,
    wav_bytes: Vec<u8>,
) -> anyhow::Result<Option<Transcript>> {
    let first = stt.transcribe(wav_bytes.clone()).await?;
    if first.complete() {
        return Ok(Some(first));
    }
    eprintln!(
        "[stt]     no words in {:?} (skipped_audio={}); retrying once",
        first.text, first.skipped_audio
    );
    let second = match stt.retranscribe(wav_bytes).await {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("[stt]     retry failed: {e:#}");
            None
        }
    };
    let chosen = pick_retry(first, second);
    match &chosen {
        Some(t) => eprintln!("[stt]     retry kept {} chars", t.text.chars().count()),
        None => eprintln!("[stt]     retry heard no words either"),
    }
    Ok(chosen)
}

/// After a retry, the most complete transcript with words: a full retry
/// beats a partial first try, which beats a partial retry.
fn pick_retry(first: Transcript, second: Option<Transcript>) -> Option<Transcript> {
    match second {
        Some(second) if second.complete() => Some(second),
        _ if has_words(&first.text) => Some(first),
        Some(second) if has_words(&second.text) => Some(second),
        _ => None,
    }
}

/// Build the configured STT backend. Groq needs GROQ_API_KEY; the local
/// whisper backend needs no key and no network (after the one-time model
/// download).
pub fn make_provider(cfg: &Config) -> anyhow::Result<Arc<dyn SttProvider>> {
    crate::userdata::ensure_starter_files();
    match cfg.stt.provider {
        SttBackend::Groq => Ok(Arc::new(groq::GroqStt::new(cfg.groq.clone())?)),
        SttBackend::Whisper => Ok(Arc::new(whisper::WhisperStt::new(
            &cfg.stt.whisper,
            &cfg.groq.language,
        )?)),
        // Shares the [stt.whisper] model knob: one "local model" setting.
        SttBackend::FasterWhisper => Ok(Arc::new(fasterwhisper::FasterWhisperStt::new(
            &cfg.stt.whisper.model,
        )?)),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    pub fn transcript(text: &str) -> Transcript {
        Transcript {
            text: text.to_string(),
            raw_json: "{}".to_string(),
            latency_ms: 0,
            skipped_audio: false,
        }
    }

    /// Answers each call with the next scripted transcript and records which
    /// method was called.
    pub struct ScriptedStt {
        answers: Mutex<Vec<Transcript>>,
        pub calls: Mutex<Vec<&'static str>>,
    }

    impl ScriptedStt {
        pub fn new(answers: Vec<Transcript>) -> Self {
            Self {
                answers: Mutex::new(answers),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn answer(&self, call: &'static str) -> anyhow::Result<Transcript> {
            self.calls.lock().unwrap().push(call);
            let mut answers = self.answers.lock().unwrap();
            anyhow::ensure!(!answers.is_empty(), "unexpected {call}");
            Ok(answers.remove(0))
        }
    }

    #[async_trait::async_trait]
    impl SttProvider for ScriptedStt {
        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
            self.answer("transcribe")
        }
        async fn retranscribe(&self, _wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
            self.answer("retranscribe")
        }
    }

    fn heard(answers: Vec<Transcript>) -> (Option<String>, Vec<&'static str>) {
        let stt = ScriptedStt::new(answers);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let heard = runtime
            .block_on(transcribe_speech(&stt, Vec::new()))
            .unwrap()
            .map(|t| t.text);
        let calls = stt.calls.lock().unwrap().clone();
        (heard, calls)
    }

    #[test]
    fn words_are_letters_or_digits() {
        assert!(has_words("Hello."));
        assert!(has_words("42"));
        assert!(has_words("- Okay"));
        assert!(!has_words(","));
        assert!(!has_words(""));
        assert!(!has_words(" , . - ... \n"));
        assert!(!has_words("- -"));
    }

    #[test]
    fn a_normal_transcript_is_kept_without_a_retry() {
        assert_eq!(
            heard(vec![transcript("Ship it, please.")]),
            (Some("Ship it, please.".to_string()), vec!["transcribe"])
        );
    }

    #[test]
    fn a_lone_comma_is_retried_once() {
        assert_eq!(
            heard(vec![
                transcript(","),
                transcript("You can use Claude for it.")
            ]),
            (
                Some("You can use Claude for it.".to_string()),
                vec!["transcribe", "retranscribe"]
            )
        );
    }

    #[test]
    fn punctuation_and_spaces_twice_is_not_caught() {
        assert_eq!(
            heard(vec![transcript(" , . - "), transcript("- -")]),
            (None, vec!["transcribe", "retranscribe"])
        );
        assert_eq!(
            heard(vec![transcript(","), transcript(",")]),
            (None, vec!["transcribe", "retranscribe"])
        );
    }

    #[test]
    fn skipped_audio_is_retried_and_the_full_retry_wins() {
        let partial = Transcript {
            skipped_audio: true,
            ..transcript("the last few words")
        };
        assert_eq!(
            heard(vec![partial, transcript("All of it. The last few words")]),
            (
                Some("All of it. The last few words".to_string()),
                vec!["transcribe", "retranscribe"]
            )
        );
    }

    #[test]
    fn a_partial_first_try_beats_a_retry_without_words() {
        let partial = Transcript {
            skipped_audio: true,
            ..transcript("the last few words")
        };
        assert_eq!(
            heard(vec![partial, transcript(",")]),
            (
                Some("the last few words".to_string()),
                vec!["transcribe", "retranscribe"]
            )
        );
    }

    #[test]
    fn a_failed_retry_falls_back_to_not_caught() {
        // The retry errors (no scripted answer left): still no paste.
        assert_eq!(
            heard(vec![transcript(",")]),
            (None, vec!["transcribe", "retranscribe"])
        );
    }
}
