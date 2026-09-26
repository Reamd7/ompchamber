//! Port of `server/lib/dictation/local/model-catalog.js` — the catalog of
//! local sherpa-onnx STT and TTS models, downloaded on demand from the
//! k2-fsa GitHub releases into the OMPChamber speech-models directory.
//!
//! The native recognizer/speaker engines are NOT part of this port (the
//! sherpa-onnx node addon cannot be linked with the allowed crates — see
//! `local.rs`), but the catalog, the install-state checks, the downloader,
//! and every route that reports or manages models are.

use std::path::PathBuf;

/// One catalog entry. STT entries carry encoder/decoder/joiner/tokens file
/// roles (`type` selects the recognizer construction path in the worker);
/// TTS entries additionally declare the languages they speak and, for
/// multi-language models, the default speaker per language.
#[derive(Debug)]
pub struct LocalModelSpec {
    pub id: &'static str,
    pub model_type: &'static str,
    pub archive_url: &'static str,
    pub extracted_dir: &'static str,
    /// (role, file-or-directory inside the extracted dir).
    pub files: &'static [(&'static str, &'static str)],
    pub description: &'static str,
    /// TTS only: languages this model speaks well.
    pub languages: &'static [&'static str],
    /// TTS only: default speaker id per language.
    pub default_speaker_by_language: &'static [(&'static str, i64)],
    /// TTS only: file roles joined with commas for sherpa-onnx `lexicon`.
    pub lexicon_roles: &'static [&'static str],
}

/// STT entries (`nemo_transducer` | `whisper`).
const fn stt(
    id: &'static str,
    model_type: &'static str,
    archive_url: &'static str,
    extracted_dir: &'static str,
    files: &'static [(&'static str, &'static str)],
    description: &'static str,
) -> LocalModelSpec {
    LocalModelSpec {
        id,
        model_type,
        archive_url,
        extracted_dir,
        files,
        description,
        languages: &[],
        default_speaker_by_language: &[],
        lexicon_roles: &[],
    }
}

/// Single-language Piper/VITS entries (a macro so every field is a literal
/// and the slices promote to `'static`).
macro_rules! vits {
    ($id:expr, $archive_url:expr, $extracted_dir:expr, $model_file:expr, $description:expr, $language:expr $(,)?) => {
        LocalModelSpec {
            id: $id,
            model_type: "vits",
            archive_url: $archive_url,
            extracted_dir: $extracted_dir,
            files: &[
                ("model", $model_file),
                ("tokens", "tokens.txt"),
                ("espeakData", "espeak-ng-data"),
            ],
            description: $description,
            languages: &[$language],
            default_speaker_by_language: &[],
            lexicon_roles: &[],
        }
    };
}

/// `LOCAL_STT_MODEL_CATALOG` (declaration order preserved).
pub static LOCAL_STT_MODEL_CATALOG: [LocalModelSpec; 4] = [
    stt(
        "parakeet-tdt-0.6b-v2-int8",
        "nemo_transducer",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
        &[
            ("encoder", "encoder.int8.onnx"),
            ("decoder", "decoder.int8.onnx"),
            ("joiner", "joiner.int8.onnx"),
            ("tokens", "tokens.txt"),
        ],
        "NVIDIA Parakeet TDT v2 (English)",
    ),
    stt(
        "parakeet-tdt-0.6b-v3-int8",
        "nemo_transducer",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
        &[
            ("encoder", "encoder.int8.onnx"),
            ("decoder", "decoder.int8.onnx"),
            ("joiner", "joiner.int8.onnx"),
            ("tokens", "tokens.txt"),
        ],
        "NVIDIA Parakeet TDT v3 (25 European languages, auto-detected)",
    ),
    stt(
        "whisper-base-int8",
        "whisper",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.tar.bz2",
        "sherpa-onnx-whisper-base",
        &[
            ("encoder", "base-encoder.int8.onnx"),
            ("decoder", "base-decoder.int8.onnx"),
            ("tokens", "base-tokens.txt"),
        ],
        "OpenAI Whisper base (multilingual, smaller and lighter)",
    ),
    stt(
        "whisper-tiny-int8",
        "whisper",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-tiny.tar.bz2",
        "sherpa-onnx-whisper-tiny",
        &[
            ("encoder", "tiny-encoder.int8.onnx"),
            ("decoder", "tiny-decoder.int8.onnx"),
            ("tokens", "tiny-tokens.txt"),
        ],
        "OpenAI Whisper tiny (multilingual, fastest and lightest)",
    ),
];

/// `LOCAL_TTS_MODEL_CATALOG` (declaration order preserved).
pub static LOCAL_TTS_MODEL_CATALOG: [LocalModelSpec; 14] = [
    LocalModelSpec {
        id: "kokoro-en-v0_19",
        model_type: "kokoro",
        archive_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-en-v0_19.tar.bz2",
        extracted_dir: "kokoro-en-v0_19",
        files: &[
            ("model", "model.onnx"),
            ("voices", "voices.bin"),
            ("tokens", "tokens.txt"),
            ("espeakData", "espeak-ng-data"),
        ],
        description: "Kokoro TTS (English, natural voices)",
        languages: &["en"],
        default_speaker_by_language: &[],
        lexicon_roles: &[],
    },
    LocalModelSpec {
        id: "kokoro-multi-lang-v1_1",
        model_type: "kokoro",
        archive_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-multi-lang-v1_1.tar.bz2",
        extracted_dir: "kokoro-multi-lang-v1_1",
        files: &[
            ("model", "model.onnx"),
            ("voices", "voices.bin"),
            ("tokens", "tokens.txt"),
            ("espeakData", "espeak-ng-data"),
            ("lexiconEnglish", "lexicon-us-en.txt"),
            ("lexiconChinese", "lexicon-zh.txt"),
        ],
        // sherpa-onnx wires this Kokoro build for Chinese and English only;
        // speakers 0-2 are English, 3-102 Chinese.
        description: "Kokoro TTS (Chinese and English, 103 voices)",
        languages: &["zh", "en"],
        default_speaker_by_language: &[("en", 0), ("zh", 3)],
        lexicon_roles: &["lexiconEnglish", "lexiconChinese"],
    },
    // The larger `ukrainian_tts-medium` build is a character-level model;
    // `vits-coqui-uk-mai` reads Cyrillic only. Lada is an espeak model:
    // small, but it reads mixed text.
    vits!(
        "piper-uk_UA-lada-x_low",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-uk_UA-lada-x_low.tar.bz2",
        "vits-piper-uk_UA-lada-x_low",
        "uk_UA-lada-x_low.onnx",
        "Piper TTS (Ukrainian)",
        "uk",
    ),
    vits!(
        "piper-de_DE-thorsten-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-de_DE-thorsten-medium.tar.bz2",
        "vits-piper-de_DE-thorsten-medium",
        "de_DE-thorsten-medium.onnx",
        "Piper TTS (German)",
        "de",
    ),
    vits!(
        "piper-fr_FR-siwis-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-fr_FR-siwis-medium.tar.bz2",
        "vits-piper-fr_FR-siwis-medium",
        "fr_FR-siwis-medium.onnx",
        "Piper TTS (French)",
        "fr",
    ),
    vits!(
        "piper-es_ES-davefx-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-es_ES-davefx-medium.tar.bz2",
        "vits-piper-es_ES-davefx-medium",
        "es_ES-davefx-medium.onnx",
        "Piper TTS (Spanish)",
        "es",
    ),
    vits!(
        "piper-it_IT-paola-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-it_IT-paola-medium.tar.bz2",
        "vits-piper-it_IT-paola-medium",
        "it_IT-paola-medium.onnx",
        "Piper TTS (Italian)",
        "it",
    ),
    vits!(
        "piper-pt_BR-faber-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-pt_BR-faber-medium.tar.bz2",
        "vits-piper-pt_BR-faber-medium",
        "pt_BR-faber-medium.onnx",
        "Piper TTS (Portuguese (Brazil))",
        "pt",
    ),
    vits!(
        "piper-pl_PL-gosia-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-pl_PL-gosia-medium.tar.bz2",
        "vits-piper-pl_PL-gosia-medium",
        "pl_PL-gosia-medium.onnx",
        "Piper TTS (Polish)",
        "pl",
    ),
    vits!(
        "piper-ru_RU-irina-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-ru_RU-irina-medium.tar.bz2",
        "vits-piper-ru_RU-irina-medium",
        "ru_RU-irina-medium.onnx",
        "Piper TTS (Russian)",
        "ru",
    ),
    vits!(
        "piper-nl_NL-pim-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-nl_NL-pim-medium.tar.bz2",
        "vits-piper-nl_NL-pim-medium",
        "nl_NL-pim-medium.onnx",
        "Piper TTS (Dutch)",
        "nl",
    ),
    vits!(
        "piper-cs_CZ-jirka-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-cs_CZ-jirka-medium.tar.bz2",
        "vits-piper-cs_CZ-jirka-medium",
        "cs_CZ-jirka-medium.onnx",
        "Piper TTS (Czech)",
        "cs",
    ),
    vits!(
        "piper-tr_TR-dfki-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-tr_TR-dfki-medium.tar.bz2",
        "vits-piper-tr_TR-dfki-medium",
        "tr_TR-dfki-medium.onnx",
        "Piper TTS (Turkish)",
        "tr",
    ),
    vits!(
        "piper-sv_SE-nst-medium",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-sv_SE-nst-medium.tar.bz2",
        "vits-piper-sv_SE-nst-medium",
        "sv_SE-nst-medium.onnx",
        "Piper TTS (Swedish)",
        "sv",
    ),
];

impl LocalModelSpec {
    /// `Object.values(spec.files)` — the install check only needs the paths.
    pub fn required_files(&self) -> Vec<&'static str> {
        self.files.iter().map(|(_, file)| *file).collect()
    }
}

pub const DEFAULT_LOCAL_STT_MODEL: &str = "parakeet-tdt-0.6b-v2-int8";
pub const DEFAULT_LOCAL_TTS_MODEL: &str = "kokoro-en-v0_19";

/// `LOCAL_STT_MODEL_IDS`.
pub fn local_stt_model_ids() -> Vec<&'static str> {
    LOCAL_STT_MODEL_CATALOG.iter().map(|spec| spec.id).collect()
}

/// `LOCAL_TTS_MODEL_IDS`.
pub fn local_tts_model_ids() -> Vec<&'static str> {
    LOCAL_TTS_MODEL_CATALOG.iter().map(|spec| spec.id).collect()
}

/// `isLocalSttModelId`.
pub fn is_local_stt_model_id(model_id: &str) -> bool {
    LOCAL_STT_MODEL_CATALOG
        .iter()
        .any(|spec| spec.id == model_id)
}

/// `isLocalTtsModelId`.
pub fn is_local_tts_model_id(model_id: &str) -> bool {
    LOCAL_TTS_MODEL_CATALOG
        .iter()
        .any(|spec| spec.id == model_id)
}

/// `isLocalModelId`: any managed local model (STT or TTS).
pub fn is_local_model_id(model_id: &str) -> bool {
    is_local_stt_model_id(model_id) || is_local_tts_model_id(model_id)
}

fn catalog_spec(model_id: &str) -> Option<&'static LocalModelSpec> {
    LOCAL_STT_MODEL_CATALOG
        .iter()
        .chain(LOCAL_TTS_MODEL_CATALOG.iter())
        .find(|spec| spec.id == model_id)
}

/// `getLocalSttModelSpec` across both catalogs; the JS throws
/// `Unknown local speech model id: <id>` for unknown ids.
pub fn local_model_spec(model_id: &str) -> Result<&'static LocalModelSpec, String> {
    catalog_spec(model_id).ok_or_else(|| format!("Unknown local speech model id: {model_id}"))
}

/// `resolveLocalTtsModelForLanguage`: prefer the caller's model when it
/// speaks the language, else the first catalog model that does; `None` when
/// nothing covers the language (callers keep the selected model).
pub fn resolve_local_tts_model_for_language(
    language: &str,
    preferred_model_id: Option<&str>,
) -> Option<String> {
    if let Some(preferred) = preferred_model_id {
        let speaks = LOCAL_TTS_MODEL_CATALOG
            .iter()
            .find(|spec| spec.id == preferred)
            .is_some_and(|spec| spec.languages.contains(&language));
        if speaks {
            return Some(preferred.to_string());
        }
    }
    LOCAL_TTS_MODEL_CATALOG
        .iter()
        .find(|spec| spec.languages.contains(&language))
        .map(|spec| spec.id.to_string())
}

/// `getLocalTtsDefaultSpeaker`: the speaker id a model should use for a
/// language when the caller's speaker was chosen for another language.
pub fn get_local_tts_default_speaker(model_id: &str, language: &str) -> Option<i64> {
    LOCAL_TTS_MODEL_CATALOG
        .iter()
        .find(|spec| spec.id == model_id)?
        .default_speaker_by_language
        .iter()
        .find(|(lang, _)| *lang == language)
        .map(|(_, speaker)| *speaker)
}

/// `getLocalSttModelDir`: `<modelsDir>/<extractedDir>`.
///
/// # Panics
/// On unknown ids (the JS throws); callers validate with
/// [`is_local_model_id`] first.
pub fn get_local_model_dir(models_dir: &std::path::Path, model_id: &str) -> PathBuf {
    let spec = local_model_spec(model_id).expect("catalog ids are validated by callers");
    models_dir.join(spec.extracted_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_membership_match_the_catalogs() {
        assert_eq!(
            local_stt_model_ids(),
            vec![
                "parakeet-tdt-0.6b-v2-int8",
                "parakeet-tdt-0.6b-v3-int8",
                "whisper-base-int8",
                "whisper-tiny-int8"
            ]
        );
        assert_eq!(local_tts_model_ids().len(), 14);
        assert_eq!(local_tts_model_ids()[0], "kokoro-en-v0_19");
        assert_eq!(
            local_tts_model_ids().last(),
            Some(&"piper-sv_SE-nst-medium")
        );
        assert!(is_local_stt_model_id("whisper-tiny-int8"));
        assert!(!is_local_stt_model_id("kokoro-en-v0_19"));
        assert!(is_local_tts_model_id("kokoro-en-v0_19"));
        assert!(is_local_model_id("whisper-tiny-int8"));
        assert!(is_local_model_id("piper-sv_SE-nst-medium"));
        assert!(!is_local_model_id("gpt-4o"));
    }

    #[test]
    fn unknown_ids_are_rejected() {
        assert_eq!(
            local_model_spec("nope").unwrap_err(),
            "Unknown local speech model id: nope"
        );
        assert!(local_model_spec("parakeet-tdt-0.6b-v2-int8").is_ok());
    }

    #[test]
    fn required_files_list_every_role() {
        let spec = local_model_spec("parakeet-tdt-0.6b-v2-int8").unwrap();
        assert_eq!(
            spec.required_files(),
            vec![
                "encoder.int8.onnx",
                "decoder.int8.onnx",
                "joiner.int8.onnx",
                "tokens.txt"
            ]
        );
        let kokoro = local_model_spec("kokoro-multi-lang-v1_1").unwrap();
        assert!(kokoro.required_files().contains(&"lexicon-us-en.txt"));
        assert_eq!(kokoro.lexicon_roles, &["lexiconEnglish", "lexiconChinese"]);
        assert_eq!(kokoro.model_type, "kokoro");
        assert_eq!(
            local_model_spec("piper-de_DE-thorsten-medium")
                .unwrap()
                .model_type,
            "vits"
        );
    }

    #[test]
    fn language_resolution_prefers_the_callers_model() {
        assert_eq!(
            resolve_local_tts_model_for_language("en", Some("kokoro-multi-lang-v1_1")),
            Some("kokoro-multi-lang-v1_1".to_string())
        );
        assert_eq!(
            resolve_local_tts_model_for_language("uk", Some("kokoro-en-v0_19")),
            Some("piper-uk_UA-lada-x_low".to_string())
        );
        assert_eq!(
            resolve_local_tts_model_for_language("uk", None),
            Some("piper-uk_UA-lada-x_low".to_string())
        );
        assert_eq!(resolve_local_tts_model_for_language("ja", None), None);
    }

    #[test]
    fn default_speaker_by_language() {
        assert_eq!(
            get_local_tts_default_speaker("kokoro-multi-lang-v1_1", "zh"),
            Some(3)
        );
        assert_eq!(
            get_local_tts_default_speaker("kokoro-multi-lang-v1_1", "en"),
            Some(0)
        );
        assert_eq!(
            get_local_tts_default_speaker("kokoro-multi-lang-v1_1", "uk"),
            None
        );
        assert_eq!(
            get_local_tts_default_speaker("piper-uk_UA-lada-x_low", "uk"),
            None
        );
    }

    #[test]
    fn model_dir_joins_the_extracted_dir() {
        let dir = get_local_model_dir(std::path::Path::new("/models"), "parakeet-tdt-0.6b-v2-int8");
        assert_eq!(
            dir,
            PathBuf::from("/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8")
        );
    }
}
