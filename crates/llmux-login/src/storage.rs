use crate::{Credentials, Error, Result};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) fn prepare(dir: &Path) -> Result<(PathBuf, String, Option<Credentials>)> {
    fs::create_dir_all(dir).map_err(|_| Error::Storage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|_| Error::Storage)?;
    }
    let dir = fs::canonicalize(dir).map_err(|_| Error::Storage)?;
    let host_path = dir.join("host_id");
    let host = match fs::read_to_string(&host_path) {
        Ok(host) => host,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let host = format!("urn:uuid:{}", uuid::Uuid::new_v4());
            let file = llmux_types::private_tempfile(&host_path, host.as_bytes())
                .map_err(|_| Error::Storage)?;
            match file.persist_noclobber(&host_path) {
                Ok(_) => host,
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    fs::read_to_string(&host_path).map_err(|_| Error::Storage)?
                }
                Err(_) => return Err(Error::Storage),
            }
        }
        Err(_) => return Err(Error::Storage),
    };
    if !host
        .strip_prefix("urn:uuid:")
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .is_some_and(|id| id.get_version_num() == 4)
    {
        return Err(Error::Storage);
    }
    let previous = match fs::read(dir.join("credentials.json")) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|_| Error::Storage)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err(Error::Storage),
    };
    Ok((dir, host, previous))
}

pub(crate) fn save(dir: &Path, credentials: &Credentials) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(credentials).map_err(|_| Error::Storage)?;
    // Stage both files before replacing either destination: a partial pair
    // (new refresh_token, old credentials.json) would corrupt the login state.
    let refresh = llmux_types::private_tempfile(
        &dir.join("refresh_token"),
        credentials.refresh_token.as_bytes(),
    )
    .map_err(|_| Error::Storage)?;
    let record = llmux_types::private_tempfile(&dir.join("credentials.json"), &bytes)
        .map_err(|_| Error::Storage)?;
    refresh
        .persist(&*dir.join("refresh_token"))
        .map_err(|_| Error::Storage)?;
    record
        .persist(&*dir.join("credentials.json"))
        .map_err(|_| Error::Storage)?;
    llmux_types::sync_dir(dir).map_err(|_| Error::Storage)
}
