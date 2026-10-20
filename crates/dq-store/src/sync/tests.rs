use dq_core::config::{AuthMethod, SshAuth, SshHop, SshTunnel};
use dq_core::engine::Engine;

use super::crypto::TEST_PARAMS;
use super::*;
use crate::MemorySecrets;

const PASS: &str = "team sync passphrase";
const DAY: i64 = 86_400_000;

struct Device {
    store: Store,
    secrets: MemorySecrets,
    key: Option<SyncKey>,
}

impl Device {
    fn new() -> Self {
        Self { store: Store::in_memory().unwrap(), secrets: MemorySecrets::default(), key: None }
    }

    fn sync(&mut self, dir: &Path, now: i64) -> SyncOutcome {
        sync_with_folder(&self.store, &self.secrets, dir, PASS, &mut self.key, TEST_PARAMS, now).unwrap()
    }

    fn add(&self, name: &str, folder_id: Option<String>) -> SavedConnection {
        let draft = ConnectionDraft {
            id: None,
            name: name.into(),
            folder_id,
            environment: Environment::Staging,
            favorite: false,
            config: ConnConfig {
                engine: Engine::Postgres,
                host: "db.internal".into(),
                database: "app".into(),
                username: "app".into(),
                ssh: Some(SshTunnel {
                    host: SshHop {
                        host: "bastion".into(),
                        port: 22,
                        username: "ops".into(),
                        auth: SshAuth::PrivateKey { path: "/keys/id_ed25519".into() },
                    },
                    jumps: Vec::new(),
                }),
                auth: AuthMethod::Password,
                ..ConnConfig::default()
            },
            password: PasswordChange::Keep,
            ssh_secrets: Vec::new(),
        };
        let saved = self.store.save_connection(&draft, true, &[true]).unwrap();
        self.secrets.set(&SavedConnection::password_key(&saved.id), "pg-hunter2-secret").unwrap();
        self.secrets.set(&SavedConnection::ssh_secret_key(&saved.id, 0), "ssh-key-passphrase-secret").unwrap();
        saved
    }

    fn rename(&self, id: &str, name: &str) {
        let c = self.store.connection(id).unwrap();
        let draft = ConnectionDraft {
            id: Some(c.id),
            name: name.into(),
            folder_id: c.folder_id,
            environment: c.environment,
            favorite: c.favorite,
            config: c.config,
            password: PasswordChange::Keep,
            ssh_secrets: Vec::new(),
        };
        self.store.save_connection(&draft, c.has_password, &c.ssh_secrets).unwrap();
    }

    fn names(&self) -> Vec<String> {
        self.store.connections().unwrap().into_iter().map(|c| c.name).collect()
    }
}

fn decrypt(dir: &Path) -> String {
    let bytes = std::fs::read(sync_file(dir)).unwrap();
    String::from_utf8(crypto::open(PASS, &bytes, None).unwrap().0).unwrap()
}

#[test]
fn two_devices_share_edits_and_deletions() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = (Device::new(), Device::new());
    let folder = a.store.create_folder("Kestrel").unwrap();
    let prod = a.add("kestrel-prod", Some(folder.id.clone()));
    let dev = a.add("kestrel-dev", None);
    assert!(a.sync(dir.path(), 1_000).wrote);

    let first = b.sync(dir.path(), 2_000);
    assert!(first.changed && !first.wrote, "b only reads: nothing of its own to add");
    assert_eq!(b.names(), ["kestrel-dev", "kestrel-prod"]);
    let on_b = b.store.connection(&prod.id).unwrap();
    assert_eq!(on_b.folder_id.as_deref(), Some(folder.id.as_str()), "ids are stable across devices");
    assert!(!on_b.has_password, "b has no secret for it yet");
    assert_eq!(on_b.ssh_secrets, [false]);

    // b enters its own password; a renames; b deletes the dev connection.
    b.secrets.set(&SavedConnection::password_key(&prod.id), "b-local-secret").unwrap();
    a.rename(&prod.id, "kestrel-production");
    assert!(a.sync(dir.path(), 3_000).wrote);
    b.store.delete_connection(&dev.id).unwrap();
    let out = b.sync(dir.path(), 4_000);
    assert!(out.changed && out.wrote);
    assert_eq!(b.names(), ["kestrel-production"]);
    assert_eq!(
        b.secrets.get(&SavedConnection::password_key(&prod.id)).unwrap().as_deref(),
        Some("b-local-secret"),
        "a remote edit never touches the local secret"
    );

    let out = a.sync(dir.path(), 5_000);
    assert!(out.changed);
    assert_eq!(a.names(), ["kestrel-production"]);
    assert!(a.secrets.get(&SavedConnection::password_key(&dev.id)).unwrap().is_none(), "deleted with its connection");

    // Nothing changed anywhere: no rewrite, no local change.
    let idle = a.sync(dir.path(), 6_000);
    assert!(!idle.changed && !idle.wrote);
}

#[test]
fn last_writer_wins_and_newer_edit_beats_older_delete() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = (Device::new(), Device::new());
    let c = a.add("orders", None);
    a.sync(dir.path(), 1_000);
    b.sync(dir.path(), 1_100);

    // Concurrent edits: b's is later and wins on both devices.
    a.rename(&c.id, "orders-a");
    b.rename(&c.id, "orders-b");
    a.sync(dir.path(), 2_000);
    b.sync(dir.path(), 2_500);
    a.sync(dir.path(), 2_600);
    assert_eq!((a.names(), b.names()), (vec!["orders-b".to_owned()], vec!["orders-b".to_owned()]));

    // a deletes, b edits afterwards (offline): the edit is newer and resurrects it.
    a.store.delete_connection(&c.id).unwrap();
    a.sync(dir.path(), 3_000);
    b.rename(&c.id, "orders-kept");
    b.sync(dir.path(), 3_500);
    a.sync(dir.path(), 3_600);
    assert_eq!(a.names(), ["orders-kept"]);
    assert!(!decrypt(dir.path()).contains("\"tombstones\":[{"), "the overridden tombstone is dropped");
}

#[test]
fn tombstones_expire_after_ninety_days() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Device::new();
    let folder = a.store.create_folder("Old").unwrap();
    a.sync(dir.path(), 1_000);
    a.store.delete_folder(&folder.id).unwrap();
    a.sync(dir.path(), 2_000);
    assert!(decrypt(dir.path()).contains(&folder.id), "tombstone recorded");
    a.sync(dir.path(), 2_000 + TOMBSTONE_TTL_MS - 1);
    assert!(decrypt(dir.path()).contains(&folder.id), "still within 90 days");
    let out = a.sync(dir.path(), 2_000 + TOMBSTONE_TTL_MS + DAY);
    assert!(out.wrote);
    assert!(!decrypt(dir.path()).contains(&folder.id), "expired tombstone removed");
}

#[test]
fn sync_file_never_contains_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Device::new();
    let c = a.add("payments", None);
    a.sync(dir.path(), 1_000);
    let raw = std::fs::read(sync_file(dir.path())).unwrap();
    for needle in ["pg-hunter2-secret", "ssh-key-passphrase-secret", "payments"] {
        assert!(!raw.windows(needle.len()).any(|w| w == needle.as_bytes()), "file is encrypted");
    }
    let json = decrypt(dir.path());
    assert!(json.contains("payments") && json.contains(&c.id));
    for forbidden in [
        "hunter2",
        "ssh-key-passphrase",
        "has_password",
        "ssh_secrets",
        "\"password\":",
        "\"passphrase\":",
        "api_key",
        "token",
    ] {
        assert!(!json.contains(forbidden), "sync payload must not contain {forbidden}: {json}");
    }
}

#[test]
fn wrong_passphrase_is_reported_and_file_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Device::new();
    a.add("x", None);
    a.sync(dir.path(), 1_000);
    let before = std::fs::read(sync_file(dir.path())).unwrap();
    let b = Device::new();
    let err = sync_with_folder(&b.store, &b.secrets, dir.path(), "not it", &mut None, TEST_PARAMS, 2_000).unwrap_err();
    assert_eq!(err.code.as_deref(), Some("sync_passphrase"));
    assert_eq!(std::fs::read(sync_file(dir.path())).unwrap(), before, "never overwritten");
}

#[test]
fn conflicted_copies_are_merged_and_placeholders_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = (Device::new(), Device::new());
    a.add("from-a", None);
    a.sync(dir.path(), 1_000);
    // b wrote at the same time; the cloud client kept b's file as a copy.
    let elsewhere = tempfile::tempdir().unwrap();
    b.add("from-b", None);
    b.sync(elsewhere.path(), 1_000);
    let copy = dir.path().join("DataQuery.connections (conflicted copy 2026-10-20).dqsync");
    std::fs::copy(sync_file(elsewhere.path()), &copy).unwrap();
    std::fs::write(dir.path().join("DataQuery.connections 2.dqsync"), b"partial").unwrap();

    let out = a.sync(dir.path(), 2_000);
    assert_eq!(a.names(), ["from-a", "from-b"]);
    assert!(out.wrote && !copy.exists(), "merged copy is removed");
    assert_eq!(out.warnings.len(), 1, "the unreadable copy is reported and kept: {:?}", out.warnings);

    let cloud = tempfile::tempdir().unwrap();
    std::fs::write(cloud.path().join(format!(".{SYNC_FILE}.icloud")), b"").unwrap();
    let err = sync_with_folder(&a.store, &a.secrets, cloud.path(), PASS, &mut None, TEST_PARAMS, 3_000).unwrap_err();
    assert!(err.message.contains("not downloaded yet"));
}
