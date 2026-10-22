use std::{fmt::Write as _, fs, path::Path};

/// The translated languages, in the order the generated catalogs list them. The
/// source text in code and `.slint` files is Simplified Chinese.
const LANGUAGES: [&str; 4] = ["zh-TW", "en", "ja", "ko"];

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/brand/timelens.ico");
    println!("cargo:rerun-if-changed=app.rc");
    embed_resource::compile("app.rc", version_macros())
        .manifest_required()
        .expect("failed to embed the Timelens application manifest");

    println!("cargo:rerun-if-changed=lang");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_bundled_translations("lang")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .expect("failed to compile the Slint interface");

    let out = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    write_catalog("messages", &Path::new(&out).join("messages.rs"));
    // Only the tests read this one: Slint bundles its own copy of these strings.
    write_catalog("timelens-app", &Path::new(&out).join("interface.rs"));
}

/// The package version for the VERSIONINFO resource in `app.rc`.
fn version_macros() -> [String; 4] {
    let part = |name: &str| std::env::var(name).expect("cargo sets the package version");
    [
        format!("TIMELENS_VERSION_MAJOR={}", part("CARGO_PKG_VERSION_MAJOR")),
        format!("TIMELENS_VERSION_MINOR={}", part("CARGO_PKG_VERSION_MINOR")),
        format!("TIMELENS_VERSION_PATCH={}", part("CARGO_PKG_VERSION_PATCH")),
        format!("TIMELENS_VERSION=\"{}\"", part("CARGO_PKG_VERSION")),
    ]
}

/// Writes one `.po` domain of every language as a table sorted by source text:
/// `(source, [forms in each of LANGUAGES])`, with no forms where untranslated.
fn write_catalog(domain: &str, output: &Path) {
    let mut table = std::collections::BTreeMap::<String, [Vec<String>; 4]>::new();
    for (index, language) in LANGUAGES.iter().enumerate() {
        let path = format!("lang/{language}/LC_MESSAGES/{domain}.po");
        println!("cargo:rerun-if-changed={path}");
        let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        for (source, forms) in parse_po(&text) {
            if !source.is_empty() && forms.iter().all(|form| !form.is_empty()) {
                table.entry(source).or_default()[index] = forms;
            }
        }
    }
    let mut code = String::from("&[\n");
    for (source, languages) in table {
        let _ = write!(code, "    ({source:?}, [");
        for forms in languages {
            let _ = write!(code, "&{forms:?}, ");
        }
        code.push_str("]),\n");
    }
    code.push(']');
    fs::write(output, code).expect("failed to write a translation catalog");
}

/// The entries of a gettext catalog: each source text and its translated
/// forms, one per plural form. A context is kept before its source, joined by
/// U+0004 as gettext does.
fn parse_po(text: &str) -> Vec<(String, Vec<String>)> {
    let mut entries = Vec::new();
    let mut context = None::<String>;
    let mut source = None::<String>;
    let mut forms = Vec::<String>::new();
    // The string that continuation lines extend: the source, a form, or none.
    let mut target = Target::Skip;
    let flush = |source: &mut Option<String>, forms: &mut Vec<String>, entries: &mut Vec<_>| {
        if let Some(source) = source.take() {
            entries.push((source, std::mem::take(forms)));
        }
        forms.clear();
    };
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = line.split_once(' ').unwrap_or((line, ""));
        if line.starts_with('"') {
            let value = unquote(line);
            match target {
                Target::Source => source.as_mut().expect("msgid comes first").push_str(&value),
                Target::Form(index) => forms[index].push_str(&value),
                Target::Skip => {}
            }
        } else if keyword == "msgctxt" {
            flush(&mut source, &mut forms, &mut entries);
            context = Some(unquote(rest));
            target = Target::Skip;
        } else if keyword == "msgid" {
            if context.is_none() {
                flush(&mut source, &mut forms, &mut entries);
            }
            source = Some(match context.take() {
                Some(context) => format!("{context}\u{4}{}", unquote(rest)),
                None => unquote(rest),
            });
            target = Target::Source;
        } else if keyword == "msgid_plural" {
            // The source has one text for any count; the plural only names the forms.
            target = Target::Skip;
        } else if let Some(index) = keyword
            .strip_prefix("msgstr")
            .map(|index| index.trim_matches(['[', ']']).parse().unwrap_or(0))
        {
            if forms.len() <= index {
                forms.resize(index + 1, String::new());
            }
            forms[index] = unquote(rest);
            target = Target::Form(index);
        } else {
            panic!("unsupported line in a translation catalog: {line}");
        }
    }
    flush(&mut source, &mut forms, &mut entries);
    entries
}

enum Target {
    Source,
    Form(usize),
    Skip,
}

fn unquote(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or_else(|| panic!("expected a quoted string: {text}"));
    let mut value = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        value.push(match c {
            '\\' => match chars.next() {
                Some('n') => '\n',
                Some('t') => '\t',
                Some(other) => other,
                None => '\\',
            },
            c => c,
        });
    }
    value
}
