//! The files that a store's caller keeps in the sandbox's directory beside the store's own, and the
//! names of the temporary files that the store owns there.

use super::FILE_NAMES;
use crate::error::{Error, Result};

/// The files that the caller of a [`SandboxStore`](super::SandboxStore) keeps in the sandbox's
/// directory beside the store's own: the configuration record whose presence means that the
/// sandbox is provisioned, if the caller keeps one, and the caller's other files, such as the
/// socket of a boot console.
///
/// The store's [lifecycle locks](super#lifecycle-locks) and its [removal](super#removal) take
/// the declaration: a holder that leaves the directory without the configuration record removes
/// its lock file as it releases the lock, and removal removes the caller's files with the store's,
/// the configuration record last. A caller that declares no configuration record, as
/// [`none`](Self::none) declares, keeps nothing that tells whether its sandbox is provisioned,
/// so the directory itself is the sandbox: its lock file stays when the lock is released, and the
/// store cannot be removed with the declaration.
///
/// Each name is one plain file name, which [`new`](Self::new) and [`with`](Self::with) check, so
/// a declaration is valid before any store is created, opened, locked, or removed with it:
///
/// - It is not empty, `.`, or `..`, and has no path separator, `/` or `\` on any host, so it
///   names no root, prefix, or path outside the directory.
/// - It has none of the characters that Windows refuses in a file name, a control character,
///   `"`, `*`, `:`, `<`, `>`, `?`, or `|`, no trailing dot or space, which Windows drops, and it is
///   not a name that Windows reserves for a device, such as `NUL` or `com1.json`, so it names the
///   same file on every host.
/// - It has only the portable file name characters, ASCII letters and digits, `.`, `_`, and `-`,
///   so that no file system takes it for another name: file systems ignore the case of other
///   letters by rules of their own, macOS's take a composed letter for the same letter followed
///   by a combining mark, and Windows also names files by short names, such as `RUNTIM~1.JSO` for
///   `runtime.json`.
/// - It is none of the store's [`FILE_NAMES`], the lock included, has neither form of a temporary
///   file that the store owns, and does not have the form of a removed file that a removal waits
///   for, `.nfs*`.
/// - It is declared once: the configuration record's name differs from the others.
///
/// Names are compared without regard to ASCII case, as Windows and macOS file systems compare
/// them.
///
/// A caller that captures the guest console in the store's console log,
/// [`CONSOLE_NAME`](super::CONSOLE_NAME), may also declare that it keeps the console of every
/// launch there, with [`keeping_console`](Self::keeping_console): a launch through a lifecycle
/// lock then leaves the log for the caller to append to, rather than removing it with the rest of
/// the previous launch's output. Removal removes it either way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallerFiles {
    /// The configuration record, whose presence means that the sandbox is provisioned.
    config: Option<String>,
    /// The caller's other files.
    others: Vec<String>,
    /// Whether the caller keeps the guest console of every launch in the store's console log.
    keeps_console: bool,
}

impl CallerFiles {
    /// Declares no files: the caller keeps nothing in the sandbox's directory, which is itself the
    /// sandbox.
    pub fn none() -> Self {
        Self::default()
    }

    /// Declares `config`, the caller's configuration record, whose presence in the directory means
    /// that the sandbox is provisioned.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a name that is not
    /// one plain file name of the caller's, as the [type](Self) describes.
    pub fn new(config: impl Into<String>) -> Result<Self> {
        let config = config.into();
        let mut files = Self::none();
        files.check(&config)?;
        files.config = Some(config);
        Ok(files)
    }

    /// Adds `name`, another file of the caller's.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a name that is not
    /// one plain file name of the caller's, as the [type](Self) describes, or that is declared
    /// already.
    pub fn with(mut self, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        self.check(&name)?;
        self.others.push(name);
        Ok(self)
    }

    /// Declares that the caller keeps the guest console of every launch in the store's console
    /// log, which launches then leave in place, as the [type](Self) describes.
    pub fn keeping_console(mut self) -> Self {
        self.keeps_console = true;
        self
    }

    /// Returns whether the caller keeps the guest console of every launch, as
    /// [`keeping_console`](Self::keeping_console) declares.
    pub fn keeps_console(&self) -> bool {
        self.keeps_console
    }

    /// Returns the name of the caller's configuration record, if it keeps one.
    pub fn config(&self) -> Option<&str> {
        self.config.as_deref()
    }

    /// Returns the names of the caller's other files.
    pub fn others(&self) -> &[String] {
        &self.others
    }

    /// Returns every declared name, in the order in which removal removes them: the other files,
    /// then the configuration record.
    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.others
            .iter()
            .map(String::as_str)
            .chain(self.config.as_deref())
    }

    /// Fails if `name` cannot be declared beside the names declared so far.
    fn check(&self, name: &str) -> Result<()> {
        match name_problem(name).or_else(|| self.collision(name)) {
            Some(problem) => Err(Error::unsupported(format!(
                "the caller's file name {name:?} {problem}"
            ))),
            None => Ok(()),
        }
    }

    /// Returns why `name` cannot be declared beside the store's files, its temporary files, and
    /// the names declared so far, comparing names, which have only ASCII characters, without
    /// regard to case.
    fn collision(&self, name: &str) -> Option<&'static str> {
        let folded = name.to_ascii_lowercase();
        if FILE_NAMES.contains(&folded.as_str()) {
            Some("is the name of one of the store's files")
        } else if is_store_temporary(&folded) || is_report_staging(&folded) {
            Some("has the form of a temporary file that the store owns")
        } else if folded.starts_with(".nfs") {
            Some("has the form of a removed file that a removal waits for")
        } else if self
            .names()
            .any(|declared| declared.eq_ignore_ascii_case(name))
        {
            Some("is declared already")
        } else {
            None
        }
    }
}

/// Returns why `name` is not one plain file name that names the same file on every host, if it is
/// not.
fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("is empty")
    } else if name == "." || name == ".." {
        Some("names a directory rather than a file in it")
    } else if name.contains(['/', '\\']) {
        Some("has a path separator")
    } else if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '*' | ':' | '<' | '>' | '?' | '|'))
    {
        Some("has a character that Windows refuses in a file name")
    } else if name.ends_with(['.', ' ']) {
        Some("ends with a dot or a space, which Windows drops")
    } else if reserves_device(name) {
        Some("is a name that Windows reserves for a device")
    } else if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Some("has a character other than ASCII letters and digits, `.`, `_`, and `-`")
    } else {
        None
    }
}

/// Returns whether Windows takes `name` for a device: a name whose part before its first dot,
/// without trailing spaces, is `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, or `COM` or `LPT`
/// and one digit, which may be a superscript one, two, or three, in any case.
fn reserves_device(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|rest| {
            let mut digits = rest.chars();
            matches!(digits.next(), Some('0'..='9' | '¹' | '²' | '³')) && digits.next().is_none()
        })
    };
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT")
}

/// Returns whether `name` is a temporary file that the store leaves beside one of its files when a
/// replacement of the file is interrupted: `.NAME.NUMBER.tmp`.
pub(super) fn is_store_temporary(name: &str) -> bool {
    name.strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(file, number)| {
            FILE_NAMES.contains(&file)
                && !number.is_empty()
                && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Returns whether `name` is the file in which the VMM stages its outcome report before it
/// publishes it as `outcome.json`, which a VMM that is killed while it writes the report leaves:
/// `.tmp` and six ASCII letters or digits, the name that the `tempfile` crate gives it.
pub(super) fn is_report_staging(name: &str) -> bool {
    name.strip_prefix(".tmp").is_some_and(|suffix| {
        suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    /// Returns the message with which declaring `config`, and then `others`, fails.
    fn refusal(config: &str, others: &[&str]) -> String {
        let declared = CallerFiles::new(config).and_then(|files| {
            others
                .iter()
                .try_fold(files, |files, &name| files.with(name))
        });
        let error = declared.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        error.message().to_owned()
    }

    #[test]
    fn declarations_keep_their_names() {
        let files = CallerFiles::new("sandbox.json")
            .unwrap()
            .with("boot.sock")
            .unwrap()
            .with(".config.json.1.tmp")
            .unwrap();
        assert_eq!(files.config(), Some("sandbox.json"));
        assert_eq!(files.others(), ["boot.sock", ".config.json.1.tmp"]);
        assert_eq!(
            files.names().collect::<Vec<_>>(),
            ["boot.sock", ".config.json.1.tmp", "sandbox.json"]
        );
        let none = CallerFiles::none().with("boot.sock").unwrap();
        assert_eq!(none.config(), None);
        assert_eq!(none.names().collect::<Vec<_>>(), ["boot.sock"]);
        assert_eq!(CallerFiles::none(), CallerFiles::default());
    }

    #[test]
    fn names_that_leave_the_directory_are_refused_on_every_host() {
        for (name, problem) in [
            ("", "is empty"),
            (".", "names a directory rather than a file in it"),
            ("..", "names a directory rather than a file in it"),
            ("/etc/passwd", "has a path separator"),
            ("../config.json", "has a path separator"),
            ("state/config.json", "has a path separator"),
            ("config.json/", "has a path separator"),
            ("C:\\config.json", "has a path separator"),
            ("..\\config.json", "has a path separator"),
            ("\\\\?\\C:\\config.json", "has a path separator"),
            ("\\", "has a path separator"),
            ("C:", "has a character that Windows refuses in a file name"),
            (
                "C:config.json",
                "has a character that Windows refuses in a file name",
            ),
            (
                "config.json:stream",
                "has a character that Windows refuses in a file name",
            ),
            (
                "config\0.json",
                "has a character that Windows refuses in a file name",
            ),
            (
                "config\n.json",
                "has a character that Windows refuses in a file name",
            ),
            (
                "config?.json",
                "has a character that Windows refuses in a file name",
            ),
            (
                "config.json.",
                "ends with a dot or a space, which Windows drops",
            ),
            (
                "config.json ",
                "ends with a dot or a space, which Windows drops",
            ),
            ("NUL", "is a name that Windows reserves for a device"),
            ("nul.json", "is a name that Windows reserves for a device"),
            ("Com1.json", "is a name that Windows reserves for a device"),
            ("lpt\u{b9}", "is a name that Windows reserves for a device"),
            ("CON .json", "is a name that Windows reserves for a device"),
            ("conout$", "is a name that Windows reserves for a device"),
        ] {
            let expected = format!("the caller's file name {name:?} {problem}");
            assert_eq!(refusal(name, &[]), expected);
            assert_eq!(refusal("config.json", &[name]), expected);
        }
        // Names that only resemble those are plain file names.
        for name in [
            "...json",
            "com10",
            "console.json",
            "nul-file",
            "lpt",
            "a_b.json",
        ] {
            CallerFiles::new(name).unwrap();
        }
    }

    #[test]
    fn names_that_a_file_system_could_take_for_others_are_refused() {
        let problem = "has a character other than ASCII letters and digits, `.`, `_`, and `-`";
        for name in [
            // The uppercase forms of `ı` and `ſ` are `I` and `S`, which `to_lowercase` does not
            // tell, so a file system that compares the uppercase forms of names could take these
            // for the store's files.
            "runt\u{131}me.json",
            "control.\u{17f}ock",
            // macOS's file systems take a composed letter for the same letter followed by a
            // combining mark.
            "caf\u{e9}.json",
            "cafe\u{301}.json",
            // Windows names `runtime.json` by this short name too on volumes with short names.
            "RUNTIM~1.JSO",
            "a b.json",
        ] {
            let expected = format!("the caller's file name {name:?} {problem}");
            assert_eq!(refusal(name, &[]), expected);
            assert_eq!(refusal("config.json", &[name]), expected);
        }
    }

    #[test]
    fn names_that_collide_are_refused_whatever_their_case() {
        for (name, problem) in [
            ("sandbox.lock", "is the name of one of the store's files"),
            ("Runtime.JSON", "is the name of one of the store's files"),
            ("outcome.json", "is the name of one of the store's files"),
            (
                ".runtime.json.12345.tmp",
                "has the form of a temporary file that the store owns",
            ),
            (
                ".SANDBOX.LOCK.1.TMP",
                "has the form of a temporary file that the store owns",
            ),
            (
                ".tmpAbC123",
                "has the form of a temporary file that the store owns",
            ),
            (
                ".TMP000000",
                "has the form of a temporary file that the store owns",
            ),
            (
                ".nfs000000001",
                "has the form of a removed file that a removal waits for",
            ),
            (
                ".NFS1",
                "has the form of a removed file that a removal waits for",
            ),
        ] {
            let expected = format!("the caller's file name {name:?} {problem}");
            assert_eq!(refusal(name, &[]), expected);
            assert_eq!(refusal("config.json", &[name]), expected);
        }
        let twice = |name: &str| format!("the caller's file name {name:?} is declared already");
        assert_eq!(
            refusal("config.json", &["config.json"]),
            twice("config.json")
        );
        assert_eq!(
            refusal("config.json", &["CONFIG.json"]),
            twice("CONFIG.json")
        );
        assert_eq!(
            refusal("config.json", &["boot.sock", "Boot.Sock"]),
            twice("Boot.Sock")
        );
        let none = CallerFiles::none().with("boot.sock").unwrap();
        assert_eq!(
            none.with("BOOT.sock").unwrap_err().message(),
            twice("BOOT.sock")
        );
    }

    #[test]
    fn store_temporaries_name_one_of_the_stores_files_and_a_number() {
        for name in [
            ".runtime.json.0.tmp",
            ".sandbox.lock.18446744073709551615.tmp",
        ] {
            assert!(is_store_temporary(name), "{name}");
        }
        for name in [
            ".config.json.1.tmp",
            ".runtime.json..tmp",
            ".runtime.json.1a.tmp",
            "runtime.json.1.tmp",
            ".runtime.json.1.tmp.tmp",
            ".tmpAbC123",
            ".Runtime.json.1.tmp",
        ] {
            assert!(!is_store_temporary(name), "{name}");
        }
    }

    #[test]
    fn report_staging_files_have_the_names_that_tempfile_gives_them() {
        for name in [".tmpAbC123", ".tmp000000", ".tmpzzzzzz"] {
            assert!(is_report_staging(name), "{name}");
        }
        // Other names are not the VMM's, whatever they resemble.
        for name in [
            ".tmpAbC12",
            ".tmpAbC1234",
            ".tmpAb-123",
            ".tmpAb.123",
            ".tmpAbC12\u{e9}",
            ".TMPAbC123",
            "tmpAbC123",
            ".tmp",
            ".runtime.json.1.tmp",
        ] {
            assert!(!is_report_staging(name), "{name}");
        }
    }
}
