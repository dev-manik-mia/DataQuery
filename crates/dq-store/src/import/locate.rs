//! Where the other apps keep their connections on macOS, Windows and Linux.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{DetectedSource, SourceApp, UserDirs, xml};

/// Most folders listed when looking for workspaces, IDE versions or projects.
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Windows,
    Linux,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

impl UserDirs {
    fn roaming(&self) -> PathBuf {
        self.app_data.clone().unwrap_or_else(|| self.home.join("AppData").join("Roaming"))
    }

    fn local(&self) -> PathBuf {
        self.local_app_data.clone().unwrap_or_else(|| self.home.join("AppData").join("Local"))
    }

    fn mac_support(&self) -> PathBuf {
        self.home.join("Library/Application Support")
    }
}

/// Connection files at fixed places.
fn fixed_files(dirs: &UserDirs) -> Vec<(SourceApp, PathBuf, &'static str)> {
    match dirs.os {
        Os::Macos => vec![
            (SourceApp::Tableplus, dirs.mac_support().join("com.tinyapp.TablePlus/Data/Connections.plist"), "Connections"),
            (SourceApp::Tableplus, dirs.mac_support().join("com.tinyapp.TablePlus-setapp/Data/Connections.plist"), "Setapp"),
            (
                SourceApp::SequelAce,
                dirs.home.join("Library/Containers/com.sequel-ace.sequel-ace/Data/Library/Application Support/Sequel Ace/Data/Favorites.plist"),
                "Favorites",
            ),
            (SourceApp::SequelPro, dirs.mac_support().join("Sequel Pro/Data/Favorites.plist"), "Favorites"),
        ],
        Os::Windows => vec![
            (SourceApp::Tableplus, dirs.local().join("com.tinyapp.TablePlus").join("Data").join("Connections.plist"), "Connections"),
            (SourceApp::Tableplus, dirs.roaming().join("com.tinyapp.TablePlus").join("Data").join("Connections.plist"), "Connections"),
        ],
        Os::Linux => Vec::new(),
    }
}

/// `DBeaver` workspace folders; each project in one has a `.dbeaver` folder.
fn dbeaver_workspaces(dirs: &UserDirs) -> Vec<PathBuf> {
    let data = match dirs.os {
        Os::Macos => vec![dirs.home.join("Library/DBeaverData")],
        Os::Windows => vec![dirs.roaming().join("DBeaverData")],
        Os::Linux => vec![
            dirs.data_home.clone().unwrap_or_else(|| dirs.home.join(".local/share")).join("DBeaverData"),
            dirs.home.join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData"),
            dirs.home.join("snap/dbeaver-ce/current/.local/share/DBeaverData"),
        ],
    };
    data.into_iter().map(|d| d.join("workspace6")).collect()
}

/// The folder holding one config folder per `JetBrains` IDE and version.
fn jetbrains_root(dirs: &UserDirs) -> PathBuf {
    match dirs.os {
        Os::Macos => dirs.mac_support().join("JetBrains"),
        Os::Windows => dirs.roaming().join("JetBrains"),
        Os::Linux => dirs.config_home.clone().unwrap_or_else(|| dirs.home.join(".config")).join("JetBrains"),
    }
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<PathBuf> =
        entries.flatten().take(MAX_ENTRIES).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    found.sort();
    found
}

/// `DataGrip2024.3` → (`DataGrip`, [2024, 3]).
fn product_version(name: &str) -> Option<(&str, Vec<u32>)> {
    let split = name.find(|c: char| c.is_ascii_digit())?;
    let (product, version) = name.split_at(split);
    let version: Option<Vec<u32>> = version.split('.').map(|p| p.parse().ok()).collect();
    Some((product, version?)).filter(|(p, _)| !p.is_empty())
}

/// Config folders of the installed `JetBrains` IDEs, the newest version of each.
pub(super) fn jetbrains_configs(dirs: &UserDirs) -> Vec<PathBuf> {
    let mut newest: HashMap<String, (Vec<u32>, PathBuf)> = HashMap::new();
    for dir in subfolders(&jetbrains_root(dirs)) {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else { continue };
        let Some((product, version)) = product_version(name) else { continue };
        if newest.get(product).is_none_or(|(v, _)| *v < version) {
            newest.insert(product.to_owned(), (version, dir.clone()));
        }
    }
    let mut configs: Vec<PathBuf> = newest.into_values().map(|(_, dir)| dir).collect();
    configs.sort();
    configs
}

/// Projects a `JetBrains` IDE opened recently (`options/recentProjects.xml`).
fn recent_projects(config: &Path, dirs: &UserDirs) -> Vec<PathBuf> {
    let file = config.join("options/recentProjects.xml");
    let Some(doc) = super::read_text(&file).ok().and_then(|t| xml::parse(&t).ok()) else { return Vec::new() };
    doc.descendants("entry")
        .into_iter()
        .filter_map(|e| e.attr("key"))
        .map(|key| PathBuf::from(dirs.expand(key, None)))
        .filter(|p| p.is_absolute())
        .take(MAX_ENTRIES)
        .collect()
}

/// Every connection file of the other apps that exists on this computer.
pub fn detect(dirs: &UserDirs) -> Vec<DetectedSource> {
    let mut found: Vec<DetectedSource> = Vec::new();
    let mut add = |app: SourceApp, path: PathBuf, label: String| {
        let path = path.to_string_lossy().into_owned();
        if !found.iter().any(|d| d.path == path) {
            found.push(DetectedSource { app, path, label });
        }
    };
    for (app, path, label) in fixed_files(dirs) {
        if path.is_file() {
            add(app, path, label.to_owned());
        }
    }
    for workspace in dbeaver_workspaces(dirs) {
        for project in subfolders(&workspace) {
            let Ok(entries) = std::fs::read_dir(project.join(".dbeaver")) else { continue };
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .take(MAX_ENTRIES)
                .map(|e| e.path())
                .filter(|p| {
                    let json = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
                    json && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("data-sources"))
                })
                .collect();
            files.sort();
            let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for file in files {
                add(SourceApp::Dbeaver, file, format!("Project {name}"));
            }
        }
    }
    for config in jetbrains_configs(dirs) {
        let ide = config.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let ide = product_version(ide).map_or_else(
            || ide.to_owned(),
            |(p, v)| format!("{p} {}", v.iter().map(u32::to_string).collect::<Vec<_>>().join(".")),
        );
        let global = config.join("options/dataSources.xml");
        if global.is_file() {
            add(SourceApp::Datagrip, global, format!("{ide} · global data sources"));
        }
        let projects = subfolders(&config.join("projects")).into_iter().chain(recent_projects(&config, dirs));
        for project in projects {
            let file = project.join(".idea/dataSources.xml");
            if file.is_file() {
                let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                add(SourceApp::Datagrip, file, format!("{ide} · project {name}"));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, fixture_dirs};
    use super::*;

    #[test]
    fn detects_every_app_in_a_mac_home() {
        let dirs = fixture_dirs();
        let found: Vec<(SourceApp, String, String)> = detect(&dirs)
            .into_iter()
            .map(|d| (d.app, d.path.strip_prefix(&*dirs.home.to_string_lossy()).unwrap_or(&d.path).to_owned(), d.label))
            .collect();
        let expected = [
            (SourceApp::Tableplus, "/Library/Application Support/com.tinyapp.TablePlus/Data/Connections.plist", "Connections"),
            (
                SourceApp::SequelAce,
                "/Library/Containers/com.sequel-ace.sequel-ace/Data/Library/Application Support/Sequel Ace/Data/Favorites.plist",
                "Favorites",
            ),
            (SourceApp::Dbeaver, "/Library/DBeaverData/workspace6/General/.dbeaver/data-sources.json", "Project General"),
            (SourceApp::Datagrip, "/Library/Application Support/JetBrains/DataGrip2024.3/options/dataSources.xml", "DataGrip 2024.3 · global data sources"),
            (SourceApp::Datagrip, "/code/shop/.idea/dataSources.xml", "DataGrip 2024.3 · project shop"),
        ]
        .map(|(a, p, l)| (a, p.to_owned(), l.to_owned()));
        assert_eq!(found, expected);
        assert!(
            fixture("home/Library/Application Support/JetBrains/DataGrip2023.2").is_dir(),
            "older versions are skipped"
        );
    }

    #[test]
    fn paths_per_os() {
        let mut dirs = fixture_dirs();
        dirs.home = PathBuf::from("/home/ada");
        dirs.os = Os::Linux;
        assert_eq!(
            dbeaver_workspaces(&dirs)[..2],
            [
                PathBuf::from("/home/ada/.local/share/DBeaverData/workspace6"),
                PathBuf::from("/home/ada/.var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6")
            ]
        );
        assert_eq!(jetbrains_root(&dirs), PathBuf::from("/home/ada/.config/JetBrains"));
        assert_eq!(fixed_files(&dirs), []);

        dirs.os = Os::Windows;
        dirs.app_data = Some(PathBuf::from("/c/Users/ada/AppData/Roaming"));
        assert_eq!(dbeaver_workspaces(&dirs), [PathBuf::from("/c/Users/ada/AppData/Roaming/DBeaverData/workspace6")]);
        assert_eq!(jetbrains_root(&dirs), PathBuf::from("/c/Users/ada/AppData/Roaming/JetBrains"));
        assert_eq!(
            fixed_files(&dirs)[0].1,
            PathBuf::from("/home/ada/AppData/Local/com.tinyapp.TablePlus/Data/Connections.plist")
        );

        assert_eq!(product_version("IntelliJIdea2024.3"), Some(("IntelliJIdea", vec![2024, 3])));
        assert_eq!(product_version("consentOptions"), None);
    }
}
