//! Named cursors from an xcursor theme (`XCURSOR_THEME`, `XCURSOR_SIZE`), looked up like libXcursor (plus the XDG
//! dirs; `Inherits` merged across dirs): the theme, the themes it inherits, then `default`. Nothing found there: the
//! built-in cursor (`render::fallback_cursor`). Lookups take the search path and the environment's values as
//! arguments, so tests point them at a temp dir.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use smithay::input::pointer::CursorIcon;
pub use xcursor::parser::Image;

/// The theme when `XCURSOR_THEME` is unset, and the theme looked in last.
pub const DEFAULT_THEME: &str = "default";
/// The cursor size (nominal pixels) when `XCURSOR_SIZE` is unset or not a whole number above 0.
pub const DEFAULT_SIZE: u32 = 24;

/// The theme and size `XCURSOR_THEME` and `XCURSOR_SIZE` name (empty is unset).
#[must_use]
pub fn theme_and_size(theme: Option<&str>, size: Option<&str>) -> (String, u32) {
    let theme = theme.filter(|t| !t.is_empty()).unwrap_or(DEFAULT_THEME);
    let size = (size.and_then(|s| s.parse().ok()))
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_SIZE);
    (theme.to_owned(), size)
}

/// Where themes are looked for, from `env` (a variable's value; empty is unset): `XCURSOR_PATH` (`:`-separated,
/// `~` the home) when set, else like libXcursor's (plus the XDG dirs): `$XDG_DATA_HOME/icons`
/// (`~/.local/share/icons`), `~/.icons`, each `$XDG_DATA_DIRS` entry's `icons` (`/usr/local/share`, `/usr/share`),
/// `/usr/share/pixmaps`.
#[must_use]
pub fn search_path(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let var = |k: &str| env(k).filter(|v| !v.is_empty());
    let home = var("HOME").map(PathBuf::from);
    // `~` and `~/...` from the home; dropped without one.
    let expand = |entry: &str| match entry.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home.as_ref().map(|h| h.join(rest.trim_start_matches('/')))
        }
        _ => Some(PathBuf::from(entry)),
    };
    if let Some(path) = var("XCURSOR_PATH") {
        return path.split(':').filter(|e| !e.is_empty()).filter_map(expand).collect();
    }
    let mut out = Vec::new();
    match var("XDG_DATA_HOME") {
        Some(data) => out.extend(expand(&data).map(|d| d.join("icons"))),
        None => out.extend(home.as_ref().map(|h| h.join(".local/share/icons"))),
    }
    out.extend(home.as_ref().map(|h| h.join(".icons")));
    let dirs = var("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    out.extend((dirs.split(':').filter(|e| !e.is_empty()).filter_map(expand)).map(|d| d.join("icons")));
    out.push(PathBuf::from("/usr/share/pixmaps"));
    out
}

/// The cursor file `name` of `theme`: in the theme's `cursors` directory on the search path, else in the themes its
/// `index.theme` files inherit (in order, depth first, each theme once), else in `default`.
#[must_use]
pub fn find(search: &[PathBuf], theme: &str, name: &str) -> Option<PathBuf> {
    let mut seen = HashSet::new();
    find_in(search, theme, name, &mut seen).or_else(|| find_in(search, DEFAULT_THEME, name, &mut seen))
}

fn find_in(search: &[PathBuf], theme: &str, name: &str, seen: &mut HashSet<String>) -> Option<PathBuf> {
    // A name with a path in it would leave the theme.
    if !seen.insert(theme.to_owned()) || [theme, name].iter().any(|s| s.contains('/') || *s == "..") {
        return None;
    }
    let dirs: Vec<PathBuf> = search.iter().map(|d| d.join(theme)).filter(|d| d.is_dir()).collect();
    if let Some(file) = (dirs.iter().map(|d| d.join("cursors").join(name))).find(|f| f.is_file()) {
        return Some(file);
    }
    let inherits: Vec<String> = dirs.iter().flat_map(|d| inherits(&d.join("index.theme"))).collect();
    inherits.iter().find_map(|parent| find_in(search, parent, name, seen))
}

/// The themes an `index.theme` names in `Inherits=` (separated by commas, semicolons or spaces).
fn inherits(index: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(index) else {
        return Vec::new();
    };
    (text.lines())
        .filter_map(|l| l.strip_prefix("Inherits")?.trim_start().strip_prefix('='))
        .flat_map(|v| v.split([',', ';', ' ', '\t']))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The image of the file at `path` whose nominal size is nearest `size` (the first such: an animation's first
/// frame); None when the file cannot be read or holds no image.
#[must_use]
pub fn load(path: &Path, size: u32) -> Option<Image> {
    let images = xcursor::parser::parse_xcursor(&std::fs::read(path).ok()?)?;
    images.into_iter().min_by_key(|i| i.size.abs_diff(size))
}

/// The theme orrery draws named cursors from, each loaded once (a miss too).
#[derive(Debug)]
pub struct CursorTheme {
    search: Vec<PathBuf>,
    theme: String,
    size: u32,
    loaded: HashMap<CursorIcon, Option<Rc<Image>>>,
}

impl CursorTheme {
    #[must_use]
    pub fn new(search: Vec<PathBuf>, theme: String, size: u32) -> Self {
        Self {
            search,
            theme,
            size,
            loaded: HashMap::new(),
        }
    }

    /// The theme the environment names, on its search path.
    #[must_use]
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok();
        let (theme, size) = theme_and_size(var("XCURSOR_THEME").as_deref(), var("XCURSOR_SIZE").as_deref());
        Self::new(search_path(var), theme, size)
    }

    /// The image for `icon`, by its name or one of its older names; None: draw the built-in cursor.
    pub fn get(&mut self, icon: CursorIcon) -> Option<Rc<Image>> {
        let Self {
            search,
            theme,
            size,
            loaded,
        } = self;
        (loaded.entry(icon))
            .or_insert_with(|| {
                let names = std::iter::once(icon.name()).chain(icon.alt_names().iter().copied());
                let file = names.into_iter().find_map(|n| find(search, theme, n))?;
                load(&file, *size).map(Rc::new)
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An xcursor file holding one `side`x`side` image per `(nominal size, side)`, each filled with its nominal size.
    fn xcursor_file(images: &[(u32, u32)]) -> Vec<u8> {
        const IMAGE: u32 = 0xfffd_0002;
        let n = u32::try_from(images.len()).expect("few images");
        let mut out = b"Xcur".to_vec();
        let put = |out: &mut Vec<u8>, words: &[u32]| words.iter().for_each(|w| out.extend(w.to_le_bytes()));
        put(&mut out, &[16, 0x1_0000, n]);
        let mut at = 16 + 12 * n;
        for &(nominal, side) in images {
            put(&mut out, &[IMAGE, nominal, at]);
            at += 36 + 4 * side * side;
        }
        for &(nominal, side) in images {
            put(&mut out, &[36, IMAGE, nominal, 1, side, side, 1, 2, 0]);
            out.extend(std::iter::repeat_n(nominal as u8, (4 * side * side) as usize));
        }
        out
    }

    /// A theme `name` under `root`: its cursors (each a 24 px image) and its `Inherits=` line, if any.
    fn theme(root: &Path, name: &str, cursors: &[&str], inherits: Option<&str>) {
        let dir = root.join(name).join("cursors");
        std::fs::create_dir_all(&dir).expect("theme dir");
        for c in cursors {
            std::fs::write(dir.join(c), xcursor_file(&[(24, 4)])).expect("cursor file");
        }
        if let Some(parents) = inherits {
            let index = format!("[Icon Theme]\nName={name}\nInherits={parents}\n");
            std::fs::write(root.join(name).join("index.theme"), index).expect("index.theme");
        }
    }

    fn from(found: Option<PathBuf>, root: &Path) -> Option<String> {
        let rel = found?.strip_prefix(root).ok()?.to_path_buf();
        Some(rel.to_string_lossy().into_owned())
    }

    #[test]
    fn the_theme_then_what_it_inherits_then_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        theme(&a, "mine", &["text"], Some("base, other"));
        theme(&a, "base", &[], None);
        // A theme's directories are merged across the search path: `base` has `wait` in the second one.
        theme(&b, "base", &["wait"], None);
        theme(&b, "other", &["grab"], Some("mine"));
        theme(&b, "default", &["text", "pointer"], None);
        let search = [a.clone(), b.clone()];
        let found = |name| from(find(&search, "mine", name), dir.path());
        assert_eq!(
            found("text").as_deref(),
            Some("a/mine/cursors/text"),
            "the theme's own first"
        );
        assert_eq!(found("wait").as_deref(), Some("b/base/cursors/wait"));
        assert_eq!(
            found("grab").as_deref(),
            Some("b/other/cursors/grab"),
            "inheriting back is no loop"
        );
        assert_eq!(found("pointer").as_deref(), Some("b/default/cursors/pointer"));
        assert_eq!(found("crosshair"), None, "the built-in cursor then");
        // A theme not installed is the default one; names never leave the theme.
        let found = from(find(&search, "missing", "pointer"), dir.path());
        assert_eq!(found.as_deref(), Some("b/default/cursors/pointer"));
        assert_eq!(find(&search, "mine", "../../default/cursors/text"), None);
        assert_eq!(find(&search, "../b/other", "grab"), None);
    }

    #[test]
    fn the_image_nearest_the_size_and_a_cache_by_icon() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cursors = dir.path().join("default").join("cursors");
        std::fs::create_dir_all(&cursors).expect("dir");
        std::fs::write(cursors.join("left_ptr"), xcursor_file(&[(24, 4), (48, 8)])).expect("file");
        std::fs::write(cursors.join("broken"), b"Xcur").expect("file");
        let file = cursors.join("left_ptr");
        let nominal = |size| load(&file, size).map(|i| (i.size, i.width, i.xhot, i.yhot));
        assert_eq!(nominal(24), Some((24, 4, 1, 2)));
        assert_eq!(nominal(32), Some((24, 4, 1, 2)));
        assert_eq!(nominal(40), Some((48, 8, 1, 2)));
        assert_eq!(load(&cursors.join("broken"), 24).map(|i| i.size), None);

        // `default` is found by its older name; each icon is loaded once.
        let mut theme = CursorTheme::new(vec![dir.path().to_path_buf()], "mine".into(), 48);
        let image = theme.get(CursorIcon::Default).expect("left_ptr");
        assert_eq!((image.size, image.pixels_rgba[0]), (48, 48));
        std::fs::remove_file(&file).expect("remove");
        let again = theme.get(CursorIcon::Default).expect("cached");
        assert!(Rc::ptr_eq(&image, &again));
        assert!(theme.get(CursorIcon::Text).is_none());
    }

    #[test]
    fn theme_and_size_come_from_the_environment_or_default() {
        assert_eq!(theme_and_size(None, None), ("default".into(), 24));
        assert_eq!(theme_and_size(Some("Adwaita"), Some("48")), ("Adwaita".into(), 48));
        assert_eq!(theme_and_size(Some(""), Some("big")), ("default".into(), 24));
        assert_eq!(theme_and_size(None, Some("0")), ("default".into(), 24));
    }

    #[test]
    fn the_search_path_is_xcursor_path_else_libxcursors() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
        };
        let paths = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(
            search_path(env(&[("HOME", "/h"), ("XCURSOR_PATH", "~/c::/x/icons:~")])),
            paths(&["/h/c", "/x/icons", "/h"])
        );
        assert_eq!(
            search_path(env(&[("HOME", "/h")])),
            paths(&[
                "/h/.local/share/icons",
                "/h/.icons",
                "/usr/local/share/icons",
                "/usr/share/icons",
                "/usr/share/pixmaps"
            ])
        );
        assert_eq!(
            search_path(env(&[
                ("XDG_DATA_HOME", "/d"),
                ("XDG_DATA_DIRS", "/s1:/s2"),
                ("XCURSOR_PATH", "")
            ])),
            paths(&["/d/icons", "/s1/icons", "/s2/icons", "/usr/share/pixmaps"]),
            "no home: nothing under it"
        );
    }
}
