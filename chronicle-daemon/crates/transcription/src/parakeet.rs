//! CHR-173 spike: Parakeet TDT through sherpa-onnx. Behind the `parakeet`
//! feature; the default build does not compile this file.

use std::path::{Path, PathBuf};

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};

use crate::{Transcriber, Transcript, TranscriptionError};

/// File paths of one NeMo transducer export, plus how to run it.
#[derive(Debug, Clone)]
pub struct ParakeetConfig {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
    /// ONNX Runtime execution provider: `cpu` or `coreml`.
    pub provider: String,
    pub num_threads: i32,
    pub name: String,
}

impl ParakeetConfig {
    /// The sherpa-onnx release layout: `encoder.int8.onnx`, `decoder.int8.onnx`,
    /// `joiner.int8.onnx`, `tokens.txt`.
    pub fn sherpa_int8(dir: &Path, name: &str) -> Self {
        Self {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
            provider: "cpu".into(),
            num_threads: 4,
            name: name.into(),
        }
    }
}

pub struct ParakeetEngine {
    recognizer: OfflineRecognizer,
    name: String,
}

fn path_str(p: &Path) -> Result<String, TranscriptionError> {
    if !p.exists() {
        return Err(TranscriptionError::ModelLoad(format!(
            "missing {}",
            p.display()
        )));
    }
    Ok(p.to_string_lossy().into_owned())
}

impl ParakeetEngine {
    pub fn load(cfg: &ParakeetConfig) -> Result<Self, TranscriptionError> {
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer = OfflineTransducerModelConfig {
            encoder: Some(path_str(&cfg.encoder)?),
            decoder: Some(path_str(&cfg.decoder)?),
            joiner: Some(path_str(&cfg.joiner)?),
        };
        config.model_config.tokens = Some(path_str(&cfg.tokens)?);
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.provider = Some(cfg.provider.clone());
        config.model_config.num_threads = cfg.num_threads;
        let recognizer = OfflineRecognizer::create(&config).ok_or_else(|| {
            TranscriptionError::ModelLoad("sherpa-onnx recognizer create failed".into())
        })?;
        Ok(Self {
            recognizer,
            name: cfg.name.clone(),
        })
    }
}

impl Transcriber for ParakeetEngine {
    /// Raw model text, trimmed. No marker filtering: the spike wants to see
    /// what Parakeet prints on silence and noise.
    fn transcribe(&self, pcm_16k_mono: &[f32]) -> Result<Transcript, TranscriptionError> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(16_000, pcm_16k_mono);
        self.recognizer.decode(&stream);
        let result = stream
            .get_result()
            .ok_or_else(|| TranscriptionError::Whisper("sherpa-onnx returned no result".into()))?;
        Ok(Transcript {
            text: result.text.trim().to_string(),
            language: None,
        })
    }

    fn variant(&self) -> &str {
        &self.name
    }
}
