//! The vault app run natively on a folder, for development: the same
//! `VaultApp` the page runs, with a backend that keeps files in that
//! folder and does not sync. Point it at a COPY of a vault (the live one
//! syncs to other devices), in a shadow session:
//!
//! ```sh
//! cp -a ~/Vault /tmp/vault-copy
//! cargo run -p cce-vault-web --example native -- /tmp/vault-copy
//! ```

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use cce_vault_web::app::store::{Backend, Store};
    use cce_vault_web::app::{install, VaultApp};
    use cce_vault_web::sync::PhoneSync;

    struct Folder(PathBuf);

    impl Backend for Folder {
        fn persist(&mut self, writes: Vec<(String, Option<Vec<u8>>)>) {
            for (path, bytes) in writes {
                let abs = self.0.join(&path);
                let r = match bytes {
                    Some(b) => abs.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&abs, b)),
                    None => std::fs::remove_file(&abs),
                };
                if let Err(e) = r {
                    eprintln!("{path}: {e}");
                }
            }
        }
        fn sync_soon(&mut self) {}
        fn connected(&self) -> bool {
            true
        }
        fn connect(&mut self) {}
        fn disconnect(&mut self) {}
        fn fetch_attachment(&mut self, _: &str) {}
        fn status(&self) -> String {
            format!("Local folder {}, not synced", self.0.display())
        }
    }

    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else if let (Ok(rel), Ok(bytes)) = (p.strip_prefix(root), std::fs::read(&p)) {
                out.push((rel.to_string_lossy().replace('\\', "/"), bytes));
            }
        }
    }

    env_logger::init();
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: native <copy of a vault>"));
    let mut files = Vec::new();
    walk(&dir, &dir, &mut files);
    install(Box::new(move |_sender| {
        let store = Rc::new(RefCell::new(Store::new(files, PhoneSync::default(), "(none: a local folder)".into())));
        (store, Box::new(Folder(dir)) as Box<dyn Backend>)
    }));
    cce_ui::engine::run::<VaultApp>();
}

#[cfg(target_arch = "wasm32")]
fn main() {}
