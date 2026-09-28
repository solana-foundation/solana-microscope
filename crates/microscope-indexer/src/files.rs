use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;

pub fn temporary_path(path: &Path) -> PathBuf {
    let mut file_name = OsString::from(".");
    file_name.push(path.file_name().expect("the path must include a file name"));
    file_name.push(".tmp");
    path.with_file_name(file_name)
}

pub fn replace(path: &Path, contents: &[u8], description: &str) -> anyhow::Result<()> {
    let temporary = temporary_path(path);
    fs::write(&temporary, contents)
        .with_context(|| format!("writing temporary {description} {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("installing {description} {}", path.display()))
}
