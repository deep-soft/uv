use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use either::Either;
use path_slash::PathExt;

/// The current working directory.
#[allow(clippy::exit, clippy::print_stderr)]
pub static CWD: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::current_dir().unwrap_or_else(|_e| {
        eprintln!("Current directory does not exist");
        std::process::exit(1);
    })
});

pub trait Simplified {
    /// Simplify a [`Path`].
    ///
    /// On Windows, this will strip the `\\?\` prefix from paths. On other platforms, it's a no-op.
    fn simplified(&self) -> &Path;

    /// Render a [`Path`] for display.
    ///
    /// On Windows, this will strip the `\\?\` prefix from paths. On other platforms, it's
    /// equivalent to [`std::path::Display`].
    fn simplified_display(&self) -> impl std::fmt::Display;

    /// Canonicalize a path without a `\\?\` prefix on Windows.
    /// For a path that can't be canonicalized (e.g. on network drive or RAM drive on Windows),
    /// this will return the absolute path if it exists.
    fn simple_canonicalize(&self) -> std::io::Result<PathBuf>;

    /// Render a [`Path`] for user-facing display.
    ///
    /// Like [`simplified_display`], but relativizes the path against the current working directory.
    fn user_display(&self) -> impl std::fmt::Display;

    /// Render a [`Path`] for user-facing display, where the [`Path`] is relative to a base path.
    ///
    /// If the [`Path`] is not relative to the base path, will attempt to relativize the path
    /// against the current working directory.
    fn user_display_from(&self, base: impl AsRef<Path>) -> impl std::fmt::Display;

    /// Render a [`Path`] for user-facing display using a portable representation.
    ///
    /// Like [`user_display`], but uses a portable representation for relative paths.
    fn portable_display(&self) -> impl std::fmt::Display;
}

impl<T: AsRef<Path>> Simplified for T {
    fn simplified(&self) -> &Path {
        dunce::simplified(self.as_ref())
    }

    fn simplified_display(&self) -> impl std::fmt::Display {
        dunce::simplified(self.as_ref()).display()
    }

    fn simple_canonicalize(&self) -> std::io::Result<PathBuf> {
        dunce::canonicalize(self.as_ref())
    }

    fn user_display(&self) -> impl std::fmt::Display {
        let path = dunce::simplified(self.as_ref());

        // If current working directory is root, display the path as-is.
        if CWD.ancestors().nth(1).is_none() {
            return path.display();
        }

        if path.as_os_str() == "" {
            // Avoid printing an empty string for the current directory
            return Path::new(".").display();
        }

        // Attempt to strip the current working directory, then the canonicalized current working
        // directory, in case they differ.
        let path = path.strip_prefix(CWD.simplified()).unwrap_or(path);

        path.display()
    }

    fn user_display_from(&self, base: impl AsRef<Path>) -> impl std::fmt::Display {
        let path = dunce::simplified(self.as_ref());

        // If current working directory is root, display the path as-is.
        if CWD.ancestors().nth(1).is_none() {
            return path.display();
        }

        // Attempt to strip the base, then the current working directory, then the canonicalized
        // current working directory, in case they differ.
        let path = path
            .strip_prefix(base.as_ref())
            .unwrap_or_else(|_| path.strip_prefix(CWD.simplified()).unwrap_or(path));

        if path.as_os_str() == "" {
            // Avoid printing an empty string for the current directory
            return Path::new(".").display();
        }

        path.display()
    }

    fn portable_display(&self) -> impl std::fmt::Display {
        let path = dunce::simplified(self.as_ref());

        // Attempt to strip the current working directory, then the canonicalized current working
        // directory, in case they differ.
        let path = path.strip_prefix(CWD.simplified()).unwrap_or(path);

        // Use a portable representation for relative paths.
        path.to_slash()
            .map(Either::Left)
            .unwrap_or_else(|| Either::Right(path.display()))
    }
}

pub trait PythonExt {
    /// Escape a [`Path`] for use in Python code.
    fn escape_for_python(&self) -> String;
}

impl<T: AsRef<Path>> PythonExt for T {
    fn escape_for_python(&self) -> String {
        self.as_ref()
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }
}

/// Normalize the `path` component of a URL for use as a file path.
///
/// For example, on Windows, transforms `C:\Users\ferris\wheel-0.42.0.tar.gz` to
/// `/C:/Users/ferris/wheel-0.42.0.tar.gz`.
///
/// On other platforms, this is a no-op.
pub fn normalize_url_path(path: &str) -> Cow<'_, str> {
    // Apply percent-decoding to the URL.
    let path = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .unwrap_or(Cow::Borrowed(path));

    // Return the path.
    if cfg!(windows) {
        Cow::Owned(
            path.strip_prefix('/')
                .unwrap_or(&path)
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        )
    } else {
        path
    }
}

/// Normalize a path, removing things like `.` and `..`.
///
/// Source: <https://github.com/rust-lang/cargo/blob/b48c41aedbd69ee3990d62a0e2006edbb506a480/crates/cargo-util/src/paths.rs#L76C1-L109C2>
///
/// CAUTION: Assumes that the path is already absolute.
///
/// CAUTION: This does not resolve symlinks (unlike
/// [`std::fs::canonicalize`]). This may cause incorrect or surprising
/// behavior at times. This should be used carefully. Unfortunately,
/// [`std::fs::canonicalize`] can be hard to use correctly, since it can often
/// fail, or on Windows returns annoying device paths.
///
/// # Errors
///
/// When a relative path is provided with `..` components that extend beyond the base directory.
/// For example, `./a/../../b` cannot be normalized because it escapes the base directory.
pub fn normalize_absolute_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().copied() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                ret.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !ret.pop() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "cannot normalize a relative path beyond the base directory: {}",
                            path.display()
                        ),
                    ));
                }
            }
            Component::Normal(c) => {
                ret.push(c);
            }
        }
    }
    Ok(ret)
}

/// Normalize a [`Path`], removing things like `.` and `..`.
pub fn normalize_path(path: &Path) -> Cow<Path> {
    // Fast path: if the path is already normalized, return it as-is.
    if path.components().all(|component| match component {
        Component::Prefix(_) | Component::RootDir | Component::Normal(_) => true,
        Component::ParentDir | Component::CurDir => false,
    }) {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(normalized(path))
    }
}

/// Normalize a [`PathBuf`], removing things like `.` and `..`.
pub fn normalize_path_buf(path: PathBuf) -> PathBuf {
    // Fast path: if the path is already normalized, return it as-is.
    if path.components().all(|component| match component {
        Component::Prefix(_) | Component::RootDir | Component::Normal(_) => true,
        Component::ParentDir | Component::CurDir => false,
    }) {
        path
    } else {
        normalized(&path)
    }
}

/// Normalize a [`Path`].
///
/// Unlike [`normalize_absolute_path`], this works with relative paths and does never error.
///
/// Note that we can theoretically go beyond the root dir here (e.g. `/usr/../../foo` becomes
/// `/../foo`), but that's not a (correctness) problem, we will fail later with a file not found
/// error with a path computed from the user's input.
///
/// # Examples
///
/// In: `../../workspace-git-path-dep-test/packages/c/../../packages/d`
/// Out: `../../workspace-git-path-dep-test/packages/d`
///
/// In: `workspace-git-path-dep-test/packages/c/../../packages/d`
/// Out: `workspace-git-path-dep-test/packages/d`
///
/// In: `./a/../../b`
fn normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                // Preserve filesystem roots and regular path components.
                normalized.push(component);
            }
            Component::ParentDir => {
                match normalized.components().next_back() {
                    None | Some(Component::ParentDir | Component::RootDir) => {
                        // Preserve leading and above-root `..`
                        normalized.push(component);
                    }
                    Some(Component::Normal(_) | Component::Prefix(_) | Component::CurDir) => {
                        // Remove inner `..`
                        normalized.pop();
                    }
                }
            }
            Component::CurDir => {
                // Remove `.`
            }
        }
    }
    normalized
}

/// Compute a path describing `path` relative to `base`.
///
/// `lib/python/site-packages/foo/__init__.py` and `lib/python/site-packages` -> `foo/__init__.py`
/// `lib/marker.txt` and `lib/python/site-packages` -> `../../marker.txt`
/// `bin/foo_launcher` and `lib/python/site-packages` -> `../../../bin/foo_launcher`
///
/// Returns `Err` if there is no relative path between `path` and `base` (for example, if the paths
/// are on different drives on Windows).
pub fn relative_to(
    path: impl AsRef<Path>,
    base: impl AsRef<Path>,
) -> Result<PathBuf, std::io::Error> {
    // Normalize both paths, to avoid intermediate `..` components.
    let path = normalize_path(path.as_ref());
    let base = normalize_path(base.as_ref());

    // Find the longest common prefix, and also return the path stripped from that prefix
    let (stripped, common_prefix) = base
        .ancestors()
        .find_map(|ancestor| {
            // Simplifying removes the UNC path prefix on windows.
            dunce::simplified(&path)
                .strip_prefix(dunce::simplified(ancestor))
                .ok()
                .map(|stripped| (stripped, ancestor))
        })
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "Trivial strip failed: {} vs. {}",
                path.simplified_display(),
                base.simplified_display()
            ))
        })?;

    // go as many levels up as required
    let levels_up = base.components().count() - common_prefix.components().count();
    let up = std::iter::repeat_n("..", levels_up).collect::<PathBuf>();

    Ok(up.join(stripped))
}

/// A path that can be serialized and deserialized in a portable way by converting Windows-style
/// backslashes to forward slashes, and using a `.` for an empty path.
///
/// This implementation assumes that the path is valid UTF-8; otherwise, it won't roundtrip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortablePath<'a>(&'a Path);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortablePathBuf(Box<Path>);

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for PortablePathBuf {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PortablePathBuf")
    }

    fn json_schema(_gen: &mut schemars::generate::SchemaGenerator) -> schemars::Schema {
        PathBuf::json_schema(_gen)
    }
}

impl AsRef<Path> for PortablePath<'_> {
    fn as_ref(&self) -> &Path {
        self.0
    }
}

impl<'a, T> From<&'a T> for PortablePath<'a>
where
    T: AsRef<Path> + ?Sized,
{
    fn from(path: &'a T) -> Self {
        PortablePath(path.as_ref())
    }
}

impl std::fmt::Display for PortablePath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.0.to_slash_lossy();
        if path.is_empty() {
            write!(f, ".")
        } else {
            write!(f, "{path}")
        }
    }
}

impl std::fmt::Display for PortablePathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.0.to_slash_lossy();
        if path.is_empty() {
            write!(f, ".")
        } else {
            write!(f, "{path}")
        }
    }
}

impl From<&str> for PortablePathBuf {
    fn from(path: &str) -> Self {
        if path == "." {
            Self(PathBuf::new().into_boxed_path())
        } else {
            Self(PathBuf::from(path).into_boxed_path())
        }
    }
}

impl From<PortablePathBuf> for Box<Path> {
    fn from(portable: PortablePathBuf) -> Self {
        portable.0
    }
}

impl From<Box<Path>> for PortablePathBuf {
    fn from(path: Box<Path>) -> Self {
        Self(path)
    }
}

impl<'a> From<&'a Path> for PortablePathBuf {
    fn from(path: &'a Path) -> Self {
        Box::<Path>::from(path).into()
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for PortablePathBuf {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for PortablePath<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::de::Deserialize<'de> for PortablePathBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s == "." {
            Ok(Self(PathBuf::new().into_boxed_path()))
        } else {
            Ok(Self(PathBuf::from(s).into_boxed_path()))
        }
    }
}

impl AsRef<Path> for PortablePathBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_url() {
        if cfg!(windows) {
            assert_eq!(
                normalize_url_path("/C:/Users/ferris/wheel-0.42.0.tar.gz"),
                "C:\\Users\\ferris\\wheel-0.42.0.tar.gz"
            );
        } else {
            assert_eq!(
                normalize_url_path("/C:/Users/ferris/wheel-0.42.0.tar.gz"),
                "/C:/Users/ferris/wheel-0.42.0.tar.gz"
            );
        }

        if cfg!(windows) {
            assert_eq!(
                normalize_url_path("./ferris/wheel-0.42.0.tar.gz"),
                ".\\ferris\\wheel-0.42.0.tar.gz"
            );
        } else {
            assert_eq!(
                normalize_url_path("./ferris/wheel-0.42.0.tar.gz"),
                "./ferris/wheel-0.42.0.tar.gz"
            );
        }

        if cfg!(windows) {
            assert_eq!(
                normalize_url_path("./wheel%20cache/wheel-0.42.0.tar.gz"),
                ".\\wheel cache\\wheel-0.42.0.tar.gz"
            );
        } else {
            assert_eq!(
                normalize_url_path("./wheel%20cache/wheel-0.42.0.tar.gz"),
                "./wheel cache/wheel-0.42.0.tar.gz"
            );
        }
    }

    #[test]
    fn test_normalize_path() {
        let path = Path::new("/a/b/../c/./d");
        let normalized = normalize_absolute_path(path).unwrap();
        assert_eq!(normalized, Path::new("/a/c/d"));

        let path = Path::new("/a/../c/./d");
        let normalized = normalize_absolute_path(path).unwrap();
        assert_eq!(normalized, Path::new("/c/d"));

        // This should be an error.
        let path = Path::new("/a/../../c/./d");
        let err = normalize_absolute_path(path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_relative_to() {
        assert_eq!(
            relative_to(
                Path::new("/home/ferris/carcinization/lib/python/site-packages/foo/__init__.py"),
                Path::new("/home/ferris/carcinization/lib/python/site-packages"),
            )
            .unwrap(),
            Path::new("foo/__init__.py")
        );
        assert_eq!(
            relative_to(
                Path::new("/home/ferris/carcinization/lib/marker.txt"),
                Path::new("/home/ferris/carcinization/lib/python/site-packages"),
            )
            .unwrap(),
            Path::new("../../marker.txt")
        );
        assert_eq!(
            relative_to(
                Path::new("/home/ferris/carcinization/bin/foo_launcher"),
                Path::new("/home/ferris/carcinization/lib/python/site-packages"),
            )
            .unwrap(),
            Path::new("../../../bin/foo_launcher")
        );
    }

    #[test]
    fn test_normalize_relative() {
        let cases = [
            (
                "../../workspace-git-path-dep-test/packages/c/../../packages/d",
                "../../workspace-git-path-dep-test/packages/d",
            ),
            (
                "workspace-git-path-dep-test/packages/c/../../packages/d",
                "workspace-git-path-dep-test/packages/d",
            ),
            ("./a/../../b", "../b"),
            ("/usr/../../foo", "/../foo"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_path(Path::new(input)), Path::new(expected));
        }
    }
}
