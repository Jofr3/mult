use std::{
    env,
    ffi::{CStr, OsStr, OsString},
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

pub(crate) fn data_home() -> io::Result<PathBuf> {
    resolve_user_base(
        env::var_os("XDG_DATA_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
        Path::new(".local/share"),
        passwd_home,
        "state",
    )
}

pub(crate) fn config_home() -> io::Result<PathBuf> {
    resolve_user_base(
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
        Path::new(".config"),
        passwd_home,
        "configuration",
    )
}

fn resolve_user_base(
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
    home_suffix: &Path,
    passwd_lookup: impl FnOnce() -> io::Result<Option<PathBuf>>,
    purpose: &str,
) -> io::Result<PathBuf> {
    if let Some(path) = absolute_nonempty(xdg) {
        return Ok(path);
    }
    if let Some(home) = absolute_nonempty(home) {
        return Ok(home.join(home_suffix));
    }
    if let Some(home) = passwd_lookup()?.filter(|path| path.is_absolute()) {
        return Ok(home.join(home_suffix));
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "cannot determine a durable {purpose} directory: set an absolute HOME or the relevant XDG directory"
        ),
    ))
}

fn absolute_nonempty(value: Option<&OsStr>) -> Option<PathBuf> {
    let value = value.filter(|value| !value.is_empty())?;
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

#[cfg(unix)]
fn passwd_home() -> io::Result<Option<PathBuf>> {
    let uid = unsafe { libc::geteuid() };
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut capacity = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    }
    .clamp(1024, 1024 * 1024);

    loop {
        let mut passwd = unsafe { std::mem::zeroed::<libc::passwd>() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; capacity];
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &mut passwd,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && capacity < 1024 * 1024 {
            capacity = (capacity * 2).min(1024 * 1024);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() || passwd.pw_dir.is_null() {
            return Ok(None);
        }

        let bytes = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
        if bytes.is_empty() {
            return Ok(None);
        }
        return Ok(Some(PathBuf::from(OsString::from_vec(bytes.to_vec()))));
    }
}

#[cfg(not(unix))]
fn passwd_home() -> io::Result<Option<PathBuf>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_xdg_path_wins() {
        let resolved = resolve_user_base(
            Some(OsStr::new("/xdg")),
            Some(OsStr::new("/home/user")),
            Path::new(".local/share"),
            || Ok(Some(PathBuf::from("/passwd"))),
            "state",
        )
        .unwrap();

        assert_eq!(resolved, Path::new("/xdg"));
    }

    #[test]
    fn relative_or_empty_environment_paths_never_select_the_cwd() {
        let resolved = resolve_user_base(
            Some(OsStr::new("relative-xdg")),
            Some(OsStr::new("")),
            Path::new(".config"),
            || Ok(Some(PathBuf::from("/passwd-home"))),
            "configuration",
        )
        .unwrap();

        assert_eq!(resolved, Path::new("/passwd-home/.config"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn missing_home_sources_return_a_clear_error() {
        let error = resolve_user_base(None, None, Path::new(".local/share"), || Ok(None), "state")
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error
            .to_string()
            .contains("cannot determine a durable state directory"));
    }
}
