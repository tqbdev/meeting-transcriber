//! Languages Soniox transcribes and translates between (any pair works).
//! Source: <https://soniox.com/docs/stt/concepts/supported-languages>

/// `(code, English name)`, sorted by name.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("af", "Afrikaans"),
    ("sq", "Albanian"),
    ("ar", "Arabic"),
    ("az", "Azerbaijani"),
    ("eu", "Basque"),
    ("be", "Belarusian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("bg", "Bulgarian"),
    ("ca", "Catalan"),
    ("zh", "Chinese"),
    ("hr", "Croatian"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("nl", "Dutch"),
    ("en", "English"),
    ("et", "Estonian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("gl", "Galician"),
    ("de", "German"),
    ("el", "Greek"),
    ("gu", "Gujarati"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hu", "Hungarian"),
    ("id", "Indonesian"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("kn", "Kannada"),
    ("kk", "Kazakh"),
    ("ko", "Korean"),
    ("lv", "Latvian"),
    ("lt", "Lithuanian"),
    ("mk", "Macedonian"),
    ("ms", "Malay"),
    ("ml", "Malayalam"),
    ("mr", "Marathi"),
    ("no", "Norwegian"),
    ("fa", "Persian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("pa", "Punjabi"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sr", "Serbian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("es", "Spanish"),
    ("sw", "Swahili"),
    ("sv", "Swedish"),
    ("tl", "Tagalog"),
    ("ta", "Tamil"),
    ("te", "Telugu"),
    ("th", "Thai"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("vi", "Vietnamese"),
    ("cy", "Welsh"),
];

/// The English name for a language code, or the code itself if unknown.
pub fn language_name(code: &str) -> &str {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or(code, |(_, name)| name)
}
