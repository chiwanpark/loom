use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

gpui_kit_assets::icon_assets!(
    ExtraIcons,
    [
        Archive,
        ArchiveRestore,
        Brain,
        CircleStop,
        Coins,
        Columns2,
        FileDiff,
        FileMinus,
        FilePen,
        FilePlus,
        FileSymlink,
        FoldVertical,
        Gauge,
        GitBranch,
        ImagePlus,
        Pencil,
        Plug,
        Rows2,
        SendHorizontal,
        ShieldCheck,
        Trash,
        Unplug
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit_assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui_kit_assets::IconName;

    use super::*;

    /// `FooBar2` -> `icons/foo-bar-2.svg`, matching the Lucide file names.
    fn icon_path(name: &str) -> String {
        let mut kebab = String::new();
        let mut prev: Option<char> = None;
        for ch in name.chars() {
            let boundary = match prev {
                Some(p) => {
                    (ch.is_ascii_uppercase()) || (ch.is_ascii_digit() && !p.is_ascii_digit())
                }
                None => false,
            };
            if boundary {
                kebab.push('-');
            }
            kebab.push(ch.to_ascii_lowercase());
            prev = Some(ch);
        }
        format!("icons/{kebab}.svg")
    }

    fn collect_icon_names(dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_icon_names(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for (idx, _) in text.match_indices("IconName::") {
                    let rest = &text[idx + "IconName::".len()..];
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if name.starts_with(|c: char| c.is_ascii_uppercase()) {
                        out.push(name);
                    }
                }
            }
        }
    }

    #[test]
    fn icon_path_heuristic_matches_icon_name() {
        assert_eq!(
            icon_path("FoldVertical"),
            IconName::FoldVertical.path().as_ref()
        );
        assert_eq!(icon_path("Columns2"), IconName::Columns2.path().as_ref());
        assert_eq!(
            icon_path("LoaderCircle"),
            IconName::LoaderCircle.path().as_ref()
        );
    }

    /// Every icon referenced by the GUI must be embedded in the binary; otherwise
    /// it renders blank and logs "could not find asset" on every frame.
    #[test]
    fn every_used_icon_is_embedded() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut names = Vec::new();
        collect_icon_names(&src, &mut names);
        names.sort();
        names.dedup();
        assert!(names.len() > 10, "icon scan found too few icons: {names:?}");

        let missing: Vec<String> = names
            .iter()
            .map(|name| icon_path(name))
            .filter(|path| !matches!(AppAssets.load(path), Ok(Some(_))))
            .collect();
        assert!(
            missing.is_empty(),
            "icons not embedded in AppAssets: {missing:?}"
        );
    }
}
